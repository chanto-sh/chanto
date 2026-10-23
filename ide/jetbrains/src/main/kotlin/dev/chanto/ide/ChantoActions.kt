package dev.chanto.ide

import com.intellij.openapi.actionSystem.ActionUpdateThread
import com.intellij.openapi.actionSystem.AnAction
import com.intellij.openapi.actionSystem.AnActionEvent
import com.intellij.openapi.components.service
import com.intellij.openapi.fileEditor.FileEditorManager
import com.intellij.openapi.ui.Messages

/**
 * The four Chanto actions, mirroring the VS Code extension's commands:
 * open a connected terminal, type an `@path` mention into it, and accept or
 * reject the proposed change the user is looking at.
 */

class OpenInTerminalAction : AnAction() {
    override fun getActionUpdateThread(): ActionUpdateThread = ActionUpdateThread.BGT

    override fun actionPerformed(event: AnActionEvent) {
        val project = event.project ?: return
        project.service<ChantoIdeService>().openTerminal()
    }
}

class InsertMentionAction : AnAction() {
    override fun getActionUpdateThread(): ActionUpdateThread = ActionUpdateThread.EDT

    override fun update(event: AnActionEvent) {
        val project = event.project
        event.presentation.isEnabledAndVisible = project != null && FileEditorManager.getInstance(project).currentFile != null
    }

    override fun actionPerformed(event: AnActionEvent) {
        val project = event.project ?: return
        val file = FileEditorManager.getInstance(project).currentFile ?: return
        val service = project.service<ChantoIdeService>()
        if (!service.hasTerminal()) {
            Messages.showWarningDialog(project, "No Chanto terminal open. Run \"Chanto: Open in Terminal\" first.", "chanto")
            return
        }
        val base = project.basePath
        val relative = if (base != null && file.path.startsWith("$base/")) file.path.substring(base.length + 1) else file.name
        // Trailing space, no newline: the mention lands in the prompt for you to complete.
        service.insertMention("@${relative.replace(" ", "\\ ")} ")
    }
}

class AcceptProposedChangeAction : AnAction() {
    override fun getActionUpdateThread(): ActionUpdateThread = ActionUpdateThread.BGT

    override fun update(event: AnActionEvent) {
        val project = event.project
        event.presentation.isEnabledAndVisible = project != null && project.service<ChantoIdeService>().ide.hasPendingChange()
    }

    override fun actionPerformed(event: AnActionEvent) {
        event.project?.service<ChantoIdeService>()?.ide?.resolveActiveChange(true)
    }
}

class RejectProposedChangeAction : AnAction() {
    override fun getActionUpdateThread(): ActionUpdateThread = ActionUpdateThread.BGT

    override fun update(event: AnActionEvent) {
        val project = event.project
        event.presentation.isEnabledAndVisible = project != null && project.service<ChantoIdeService>().ide.hasPendingChange()
    }

    override fun actionPerformed(event: AnActionEvent) {
        event.project?.service<ChantoIdeService>()?.ide?.resolveActiveChange(false)
    }
}
