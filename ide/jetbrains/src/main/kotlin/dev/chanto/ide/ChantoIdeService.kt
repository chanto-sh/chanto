package dev.chanto.ide

import com.intellij.openapi.Disposable
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.components.service
import com.intellij.openapi.diagnostic.thisLogger
import com.intellij.openapi.editor.EditorFactory
import com.intellij.openapi.editor.event.SelectionEvent
import com.intellij.openapi.editor.event.SelectionListener
import com.intellij.openapi.fileEditor.FileEditorManagerEvent
import com.intellij.openapi.fileEditor.FileEditorManagerListener
import com.intellij.openapi.project.Project
import com.intellij.openapi.startup.ProjectActivity
import com.intellij.openapi.util.Disposer
import com.intellij.openapi.util.SystemInfo
import com.intellij.terminal.ui.TerminalWidget
import com.intellij.util.Alarm
import org.jetbrains.plugins.terminal.TerminalToolWindowManager

/**
 * Runs the Chanto IDE bridge for a project: binds the loopback WebSocket,
 * writes the lockfile Chanto discovers, and pushes selection changes so the
 * agent can use what you have highlighted.
 *
 * Started by [ChantoStartupActivity] and stopped when the project closes.
 */
class ChantoIdeService(private val project: Project) : Disposable {

    private val log = thisLogger()

    /** The editor side of the bridge; also used by the diff actions. */
    val ide = IntelliJIde(project)

    private val server = IdeServer(ide, "IntelliJ IDEA", log = { log.info("chanto IDE bridge: $it") })

    /** Coalesces the flood of selection events into one notification. */
    private val alarm = Alarm(Alarm.ThreadToUse.SWING_THREAD, this)

    private val listeners = Disposer.newDisposable("chanto-ide-listeners")

    @Volatile
    private var running = false

    @Volatile
    private var terminal: TerminalWidget? = null

    val port: Int get() = server.port

    fun start() {
        if (running) return
        if (ApplicationManager.getApplication().isHeadlessEnvironment) return // tests, `idea inspect`, …
        running = true
        try {
            val bound = server.start()
            server.writeLockfile(ide.workspaceFolders())
            log.info("chanto IDE bridge listening on 127.0.0.1:$bound")
            listen()
        } catch (e: Exception) {
            running = false
            log.warn("could not start the Chanto IDE bridge", e)
        }
    }

    override fun dispose() {
        running = false
        Disposer.dispose(listeners)
        server.stop()
        log.info("chanto IDE bridge stopped")
    }

    /** Whether "Open in Terminal" has run and the widget is still around. */
    fun hasTerminal(): Boolean = terminal != null

    /**
     * Open an integrated terminal running Chanto, tagged with this bridge's
     * port so the agent connects to this window rather than guessing.
     */
    fun openTerminal() {
        val widget = TerminalToolWindowManager.getInstance(project)
            .createShellWidget(project.basePath, "chanto", false, true)
        terminal = widget
        runCatching { widget.addTerminationCallback({ terminal = null }, this) }
        val command = when {
            !running || port <= 0 -> "chanto"
            SystemInfo.isWindows -> "chanto"
            else -> "export CHANTO_IDE_PORT=$port; chanto"
        }
        runCatching { widget.sendCommandToExecute(command) }
            .onFailure { log.warn("could not start Chanto in the terminal", it) }
    }

    /** Type [text] into the Chanto terminal without pressing Enter. */
    fun insertMention(text: String) {
        val widget = terminal ?: return
        runCatching { widget.ttyConnector?.write(text) }
            .onFailure { log.warn("could not type into the Chanto terminal", it) }
    }

    // ------------------------------------------------------------ internals

    private fun listen() {
        EditorFactory.getInstance().eventMulticaster.addSelectionListener(
            object : SelectionListener {
                override fun selectionChanged(event: SelectionEvent) = schedulePublish()
            },
            listeners,
        )
        project.messageBus.connect(listeners).subscribe(
            FileEditorManagerListener.FILE_EDITOR_MANAGER,
            object : FileEditorManagerListener {
                override fun selectionChanged(event: FileEditorManagerEvent) = schedulePublish()
            },
        )
    }

    private fun schedulePublish() {
        if (!running) return
        alarm.cancelAllRequests()
        alarm.addRequest({ publish() }, 150)
    }

    private fun publish() {
        if (!running) return
        server.notify("selectionChanged", ide.getSelection() ?: Json.Null)
    }

    companion object {
        fun getInstance(project: Project): ChantoIdeService = project.service()
    }
}

/** Starts the bridge once the project is open. */
class ChantoStartupActivity : ProjectActivity {
    override suspend fun execute(project: Project) {
        ChantoIdeService.getInstance(project).start()
    }
}
