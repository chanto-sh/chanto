package dev.chanto.ide

import com.intellij.codeInsight.daemon.impl.DaemonCodeAnalyzerEx
import com.intellij.codeInsight.daemon.impl.HighlightInfo
import com.intellij.diff.DiffContentFactory
import com.intellij.diff.DiffManager
import com.intellij.diff.DiffRequestPanel
import com.intellij.diff.requests.SimpleDiffRequest
import com.intellij.diff.contents.DiffContent
import com.intellij.lang.annotation.HighlightSeverity
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.editor.Editor
import com.intellij.openapi.fileEditor.FileDocumentManager
import com.intellij.openapi.fileEditor.FileEditorManager
import com.intellij.openapi.fileEditor.OpenFileDescriptor
import com.intellij.openapi.fileTypes.FileType
import com.intellij.openapi.fileTypes.FileTypeManager
import com.intellij.openapi.project.Project
import com.intellij.openapi.roots.ProjectRootManager
import com.intellij.openapi.ui.DialogWrapper
import com.intellij.openapi.util.Disposer
import com.intellij.openapi.vfs.VfsUtil
import com.intellij.openapi.vfs.VirtualFile
import com.intellij.util.Processor
import java.awt.event.ActionEvent
import java.nio.file.Paths
import java.util.concurrent.CopyOnWriteArrayList
import javax.swing.AbstractAction
import javax.swing.Action
import javax.swing.JComponent

/**
 * [IdeApi] backed by the IntelliJ platform for one project.
 *
 * Every platform call is marshalled onto the EDT: the bridge answers requests
 * from the socket threads, and the editor model is not safe to touch off it.
 * `openDiff` shows a modal diff and blocks until the user accepts or rejects,
 * which is what lets Chanto apply the user's version of a proposed change.
 */
class IntelliJIde(private val project: Project) : IdeApi {

    /** A proposed-change dialog that is waiting for the user. */
    private class PendingChange(val path: String, val dialog: DialogWrapper)

    private val pending = CopyOnWriteArrayList<PendingChange>()

    override fun getSelection(): Json? = onEdt {
        val manager = FileEditorManager.getInstance(project)
        val editor = manager.selectedTextEditor ?: return@onEdt null
        val file = localFile(editor) ?: return@onEdt null
        val document = editor.document
        val selection = editor.selectionModel
        val selected = selection.hasSelection()
        val start = if (selected) selection.selectionStart else editor.caretModel.offset
        val end = if (selected) selection.selectionEnd else start
        json(
            "file" to file.path,
            "text" to (if (selected) selection.selectedText ?: "" else ""),
            "startLine" to document.getLineNumber(start.coerceIn(0, document.textLength)) + 1,
            "endLine" to document.getLineNumber(end.coerceIn(0, document.textLength)) + 1,
            "isEmpty" to !selected,
        )
    }

    override fun getOpenEditors(): Json = onEdt {
        val manager = FileEditorManager.getInstance(project)
        val current = manager.currentFile
        val documents = FileDocumentManager.getInstance()
        Json.of(
            manager.openFiles
                .filter { it.isInLocalFileSystem }
                .map { file ->
                    mapOf(
                        "file" to file.path,
                        "isActive" to (file == current),
                        "isDirty" to documents.isFileModified(file),
                    )
                },
        )
    }

    override fun getWorkspaceFolders(): Json = Json.of(workspaceFolders())

    /** The project's content roots, falling back to the base path. */
    fun workspaceFolders(): List<String> = onEdt {
        ProjectRootManager.getInstance(project).contentRoots
            .mapNotNull { it.path.takeIf { path -> path.isNotEmpty() } }
            .ifEmpty { listOfNotNull(project.basePath) }
    }

    override fun getDiagnostics(path: String?): Json = onEdt {
        if (path != null) {
            val file = findFile(path)
            return@onEdt Json.of(if (file == null) emptyList<Json>() else listOf(diagnosticsFor(file)))
        }
        // Every open file that has something to report.
        val reports = FileEditorManager.getInstance(project).openFiles
            .filter { it.isInLocalFileSystem }
            .map { diagnosticsFor(it) }
            .filter { it["diagnostics"]?.asArray?.isNotEmpty() == true }
        Json.of(reports)
    }

    override fun openFile(path: String, line: Int?, endLine: Int?): Json = onEdt {
        val file = findFile(path) ?: throw IllegalArgumentException("no such file: $path")
        val manager = FileEditorManager.getInstance(project)
        val descriptor = if (line != null) {
            OpenFileDescriptor(project, file, (line - 1).coerceAtLeast(0), 0)
        } else {
            OpenFileDescriptor(project, file)
        }
        val editor = manager.openTextEditor(descriptor, true)
        if (editor != null && line != null) {
            val document = editor.document
            val first = (line - 1).coerceIn(0, document.lineCount - 1)
            val last = ((endLine ?: line) - 1).coerceIn(first, document.lineCount - 1)
            editor.selectionModel.setSelection(document.getLineStartOffset(first), document.getLineEndOffset(last))
        }
        json("ok" to true)
    }

    override fun openDiff(path: String, newContents: String, title: String?): Json {
        var result: Json = json("accepted" to false)
        onEdt { result = showProposedChange(path, newContents, title) }
        return result
    }

    override fun closeDiffs(path: String?) {
        for (change in pending) {
            if (path == null || change.path == path) reject(change)
        }
    }

    /** Whether a proposed change is waiting for the user (drives action enablement). */
    fun hasPendingChange(): Boolean = pending.isNotEmpty()

    /** Accept or reject the change the user is looking at, for the diff toolbar actions. */
    fun resolveActiveChange(accepted: Boolean) {
        val change = activeChange() ?: return
        if (accepted) change.dialog.close(DialogWrapper.OK_EXIT_CODE) else reject(change)
    }

    // ------------------------------------------------------------ internals

    private fun showProposedChange(path: String, newContents: String, title: String?): Json {
        val target = findFile(path)
        val fileType: FileType = target?.fileType
            ?: FileTypeManager.getInstance().getFileTypeByFileName(fileName(path))
        val factory = DiffContentFactory.getInstance()
        // The original side is the file on disk, or an empty content for a file that doesn't exist yet.
        val original: DiffContent = if (target != null) factory.createDocument(project, target)!! else factory.createEmpty()!!
        val proposed = factory.createEditable(project, newContents, fileType)
        val dialogTitle = title ?: "${fileName(path)}: current ↔ proposed (Chanto)"
        val request = SimpleDiffRequest(dialogTitle, original, proposed, "current", "proposed")

        val disposable = Disposer.newDisposable("chanto-proposed-change")
        val panel = DiffManager.getInstance().createRequestPanel(project, disposable, null)
        panel.setRequest(request)
        val dialog = ProposedChangeDialog(project, panel, dialogTitle)
        val change = PendingChange(path, dialog)
        pending.add(change)
        try {
            return if (dialog.showAndGet()) {
                // The user may have edited the proposed side before accepting.
                json("accepted" to true, "contents" to proposed.document.text)
            } else {
                json("accepted" to false)
            }
        } finally {
            pending.remove(change)
            Disposer.dispose(disposable)
        }
    }

    private fun reject(change: PendingChange) {
        val application = ApplicationManager.getApplication()
        if (application.isDispatchThread) {
            change.dialog.doCancelAction()
        } else {
            application.invokeLater { change.dialog.doCancelAction() }
        }
    }

    private fun activeChange(): PendingChange? {
        if (pending.isEmpty()) return null
        val active = java.awt.KeyboardFocusManager.getCurrentKeyboardFocusManager().activeWindow
        if (active != null) {
            pending.firstOrNull { it.dialog.window === active }?.let { return it }
        }
        return pending.last()
    }

    private fun diagnosticsFor(file: VirtualFile): Json {
        val document = FileDocumentManager.getInstance().getDocument(file)
        val entries = mutableListOf<Json>()
        if (document != null) {
            DaemonCodeAnalyzerEx.processHighlights(
                document,
                project,
                HighlightSeverity.INFORMATION,
                0,
                document.textLength,
                Processor { info: HighlightInfo ->
                    info.description?.let { message ->
                        val line = document.getLineNumber(info.startOffset.coerceIn(0, document.textLength))
                        entries.add(
                            json(
                                "line" to line + 1,
                                "character" to (info.startOffset - document.getLineStartOffset(line) + 1),
                                "severity" to severityName(info.severity),
                                "message" to message,
                                "source" to info.inspectionToolId,
                                "code" to info.toolId?.toString(),
                            ),
                        )
                    }
                    true
                },
            )
        }
        return json("file" to file.path, "diagnostics" to entries)
    }

    /**
     * Map an IntelliJ severity onto the protocol's four levels. `HighlightSeverity.INFO`
     * is a deprecated constant, but its name is still what inspections report at that level.
     */
    private fun severityName(severity: HighlightSeverity): String = when {
        severity >= HighlightSeverity.ERROR -> "error"
        severity >= HighlightSeverity.WARNING -> "warning"
        severity.name == "INFO" -> "info"
        else -> "hint"
    }

    private fun localFile(editor: Editor): VirtualFile? =
        FileDocumentManager.getInstance().getFile(editor.document)?.takeIf { it.isInLocalFileSystem }

    private fun findFile(path: String): VirtualFile? = VfsUtil.findFile(Paths.get(path), true)

    private fun fileName(path: String): String = Paths.get(path).fileName?.toString() ?: path

    /**
     * Run [block] on the EDT (directly when already there) and return its value.
     * `openDiff` relies on this blocking until the modal dialog closes.
     */
    private fun <T> onEdt(block: () -> T): T {
        val application = ApplicationManager.getApplication()
        if (application.isDispatchThread) return block()
        var value: T? = null
        application.invokeAndWait { value = block() }
        @Suppress("UNCHECKED_CAST")
        return value as T
    }

    /** The modal diff with Accept/Reject buttons that Chanto's `openDiff` waits on. */
    private class ProposedChangeDialog(
        project: Project,
        private val panel: DiffRequestPanel,
        title: String,
    ) : DialogWrapper(project, true) {
        init {
            this.title = title
            isResizable = true
            setSize(1000, 720)
            init()
        }

        override fun createCenterPanel(): JComponent = panel.component

        override fun getPreferredFocusedComponent(): JComponent? = panel.preferredFocusedComponent

        override fun createActions(): Array<Action> = arrayOf(
            action("Accept") { close(OK_EXIT_CODE) },
            action("Reject") { close(CANCEL_EXIT_CODE) },
        )

        private fun action(name: String, perform: () -> Unit): Action = object : AbstractAction(name) {
            override fun actionPerformed(event: ActionEvent) = perform()
        }
    }
}
