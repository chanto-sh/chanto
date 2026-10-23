package dev.chanto.ide

/**
 * The lockfile Chanto reads to find this IDE. Mirrors the schema in
 * `ide/PROTOCOL.md`.
 */
data class LockInfo(
    val pid: Long,
    val port: Int,
    val workspaceFolders: List<String>,
    val ideName: String,
    val authToken: String,
    val transport: String = "ws",
    val protocolVersion: Int = 1,
) {
    fun toJson(): Json = json(
        "pid" to pid,
        "port" to port,
        "workspaceFolders" to workspaceFolders,
        "ideName" to ideName,
        "transport" to transport,
        "authToken" to authToken,
        "protocolVersion" to protocolVersion,
    )
}

/**
 * The editor operations the bridge exposes. [IntelliJIde] implements this
 * against the IntelliJ platform; the bridge core only depends on this
 * interface, so it can be driven by a fake in tests.
 *
 * Return values are already in the protocol's JSON shape (see `ide/PROTOCOL.md`),
 * and `openDiff` blocks until the user accepts or rejects.
 */
interface IdeApi {
    /** `getSelection` — or `null` when no file editor is active. */
    fun getSelection(): Json?

    /** `getOpenEditors` */
    fun getOpenEditors(): Json

    /** `getWorkspaceFolders` */
    fun getWorkspaceFolders(): Json

    /** `getDiagnostics`, for one file or every file when [path] is null. */
    fun getDiagnostics(path: String?): Json

    /** `openFile` */
    fun openFile(path: String, line: Int?, endLine: Int?): Json

    /** `openDiff` — blocks until the user decides; `{accepted, contents?}`. */
    fun openDiff(path: String, newContents: String, title: String?): Json

    /** `closeDiffs` — resolve any pending diff(s) as rejected. */
    fun closeDiffs(path: String?)
}
