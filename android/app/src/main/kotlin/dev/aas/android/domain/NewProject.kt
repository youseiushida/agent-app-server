package dev.aas.android.domain

import dev.aas.android.protocol.FsRoot

/** Why a folder name cannot be used for a new project (checked again by the daemon). */
enum class NameProblem {
    Empty,

    /** Path separators or one of `:*?"<>|`, or control characters (protocol.md `project/create`). */
    InvalidCharacters,

    /** `.` / `..`, a trailing dot or space (Windows drops them), or a reserved device name. */
    NotAllowed,
}

/** Folder names for new projects. */
object ProjectNames {
    private const val FORBIDDEN = "\\/:*?\"<>|"

    /** Device names Windows reserves in every folder (CON, NUL, COM1 …), matched case-insensitively. */
    private val RESERVED = setOf("CON", "PRN", "AUX", "NUL") + (1..9).flatMap { listOf("COM$it", "LPT$it") }

    fun validate(name: String): NameProblem? {
        if (name.isBlank()) return NameProblem.Empty
        if (name.any { it in FORBIDDEN || it.isISOControl() }) return NameProblem.InvalidCharacters
        if (name == "." || name == ".." || name.endsWith('.') || name.endsWith(' ') || name.startsWith(' ')) return NameProblem.NotAllowed
        if (name.substringBefore('.').uppercase() in RESERVED) return NameProblem.NotAllowed
        return null
    }

    /**
     * The folder name `git clone <url>` would choose: the last path component without a
     * trailing `/` and `.git` (for `https://host/owner/app.git` and `git@host:owner/app.git`
     * alike). `null` when the URL has no usable component; the user types a name then.
     */
    fun fromCloneUrl(url: String): String? {
        var s = url.trim().trimEnd('/')
        if (s.endsWith(".git")) s = s.dropLast(".git".length)
        s = s.trimEnd('/')
        val cut = maxOf(s.lastIndexOf('/'), s.lastIndexOf(':'), s.lastIndexOf('\\'))
        val name = s.substring(cut + 1)
        return name.takeIf { it.isNotEmpty() && validate(it) == null }
    }

    /**
     * A URL `git clone` accepts: a URL with a scheme (`https://`, `ssh://`, `git://`, `file://`)
     * or the scp-like `user@host:path`. Only a hint before sending; git decides.
     */
    fun looksLikeCloneUrl(url: String): Boolean {
        val s = url.trim()
        if (s.isEmpty() || s.any { it.isWhitespace() }) return false
        if (Regex("^[a-zA-Z][a-zA-Z0-9+.-]*://.+").matches(s)) return true
        return Regex("^[^@/]+@[^:/]+:.+").matches(s)
    }
}

/**
 * Paths the daemon reported (`fs/roots`, `fs/list`). They are the PC's paths: the app never
 * interprets them beyond walking up and down the folders the daemon listed.
 */
object ServerPaths {
    /** The separator the path uses (the daemon runs on Windows: `\`; `/` for other forms). */
    fun separatorOf(path: String): Char = if ('\\' in path) '\\' else '/'

    /** Windows paths compare case-insensitively (CLAUDE.md); others exactly. */
    fun same(a: String, b: String): Boolean {
        val x = a.trimEnd('\\', '/')
        val y = b.trimEnd('\\', '/')
        return if (separatorOf(a) == '\\' || separatorOf(b) == '\\') x.equals(y, ignoreCase = true) else x == y
    }

    /** [path] is [root] or inside it. */
    fun isWithin(path: String, root: String): Boolean {
        if (same(path, root)) return true
        val base = root.trimEnd('\\', '/') + separatorOf(root)
        return if (separatorOf(root) == '\\') path.startsWith(base, ignoreCase = true) else path.startsWith(base)
    }

    /** The folder [name] inside [parent] (for `fs/mkdir`). */
    fun child(parent: String, name: String): String {
        val separator = separatorOf(parent)
        return if (parent.endsWith(separator)) parent + name else parent + separator + name
    }

    /** The last component of [path] (the folder's name). */
    fun name(path: String): String {
        val trimmed = path.trimEnd('\\', '/')
        return trimmed.substring(trimmed.lastIndexOfAny(charArrayOf('\\', '/')) + 1).ifEmpty { trimmed }
    }

    /** The folder above [path], or `null` when [path] is one of [roots] (or outside all of them). */
    fun parent(path: String, roots: List<FsRoot>): String? {
        if (roots.any { same(it.path, path) }) return null
        val trimmed = path.trimEnd('\\', '/')
        val cut = trimmed.lastIndexOfAny(charArrayOf('\\', '/'))
        if (cut <= 0) return null
        var up = trimmed.substring(0, cut)
        // "C:" alone is the current directory of drive C, not its root.
        if (up.length == 2 && up[1] == ':') up += '\\'
        return up.takeIf { candidate -> roots.any { isWithin(candidate, it.path) } }
    }

    /** The root that contains [path]. */
    fun rootOf(path: String, roots: List<FsRoot>): FsRoot? = roots.filter { isWithin(path, it.path) }.maxByOrNull { it.path.length }
}
