use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::error::{AppError, Result};
use crate::agent::rlm_cache::KernelInventory;
use crate::agent::tool_registry::{Tool, ToolContext, ToolDefinition, ToolResult};

/// Read-only security guard template (Python). References `_rlm_project_root`
/// and `_rlm_allowlist` which are injected at spawn time via a bootstrap snippet
/// (see `compose_kernel_bootstrap`). The guard blocks ALL writes and blocks
/// reads outside `project_root ∪ allowlist`, plus a hardcoded sensitive denylist.
///
/// The ENTIRE guard is installed inside one function so every saved original
/// (`_orig_open`, `_orig_os_open`, ...) stays a function-local variable. If they
/// were module-level names they would be exec'd into the SAME `globals()` the
/// model code runs in, letting the model read any file with `_orig_open(...)`
/// (e.g. the hub master token). Only the whitelisted guard helpers are exported.
/// `importlib.reload` and fresh re-imports of patched modules are blocked too,
/// so `importlib.reload(os)` cannot restore the unpatched `os.remove`/`os.system`.
///
/// Hardening layers beyond the original design (each closes a concrete bypass):
///   1. `_rlm_allowlist` is an immutable `_rlm_Allowlist` container (no mutation
///      methods, `__slots__`, tuple-backed). Even if model code reaches the real
///      allowlist object through `helper.__globals__`, `_rlm_allowlist.append('/')`
///      fails; the Rust side replaces the whole container on sync instead.
///   2. `os.environ` is replaced with a sanitized COPY (allowlist of non-secret
///      variables), so `os.environ`/`os.getenv` no longer expose API keys, tokens
///      or DATABASE_URLs inherited from the IDE process.
///   3. `os.kill`/`os.killpg`/`os.abort`/`os._exit` and the site builtins
///      `exit`/`quit` are blocked so model code cannot kill the kernel (or, via
///      `os.kill(os.getppid(), 9)`, the parent IDE process).
///   4. Metadata probes (`os.stat`, `os.lstat`, `os.readlink`, `os.path.exists/
///      isfile/isdir/lexists/islink/getsize/getatime/getmtime/getctime`) are
///      scope-checked, so `os.stat('~/.ssh/id_rsa').st_size` no longer leaks.
///   5. `import builtins` is refused (the module used to hand out every patched
///      function, whose `__closure__` leaks the saved `_orig_*` originals).
///   6. Model code runs with a per-execution COPY of builtins (never the shared
///      module) with the introspection / process-exit primitives stripped, so
///      `__builtins__['open'] = ...` cannot poison later calls.
///
/// Residual risk (documented, not closable in-process): the whitelisted helpers
/// (`_rlm_load` etc.) are ordinary Python functions, and Python exposes
/// `__globals__`/`__closure__`/`__defaults__` to direct attribute access. A
/// hostile model can therefore still reach the patched `os`/`open` and, through
/// their closures, the saved originals. True isolation requires the kernel to
/// run in a separate OS sandbox with no secrets on disk. `_rlm_install_guard()`
/// is kept callable and is re-run before every `execute_user_code` so any
/// module/builtins mutation a previous snippet performed is rolled back.
const READONLY_GUARD_PY: &str = r#"
import io as _io, sys as _sys, importlib as _importlib

class ReadOnlyError(PermissionError):
    pass

class _rlm_Allowlist:
    __slots__ = ('_roots',)
    def __init__(self, roots):
        _seen = []
        for r in roots:
            try:
                _rp = os.path.realpath(str(r))
            except Exception:
                _rp = str(r)
            if _rp not in _seen:
                _seen.append(_rp)
        self._roots = tuple(_seen)
    def __contains__(self, item):
        return item in self._roots
    def __iter__(self):
        return iter(self._roots)
    def __len__(self):
        return len(self._roots)

# Builtin names stripped from the per-execution user namespace. `open` is the
# only file-read primitive (replaced by the `_rlm_*` helpers), `__import__` stays
# (standard-library imports are fine), and the process-exit / interactivity /
# code-eval helpers are removed. Reflection helpers (`getattr`, `globals`, ...)
# stay: they only see the throwaway per-exec namespace, never the guard state.
_RLM_BLOCKED_BUILTINS = frozenset({
    'open', 'input', 'exit', 'quit', 'help', 'breakpoint', 'execfile',
    'raw_input', 'compile', 'eval', 'exec', 'memoryview',
})

# Only non-secret, code-relevant environment variables survive. API keys,
# cloud credentials, and app secrets inherited from the IDE process must never
# be visible to model code.
_RLM_ENV_ALLOW = frozenset({
    'PATH', 'HOME', 'PWD', 'OLDPWD', 'SHELL', 'USER', 'LOGNAME', 'LANG',
    'LANGUAGE', 'LC_ALL', 'LC_CTYPE', 'LC_COLLATE', 'LC_MESSAGES',
    'LC_MONETARY', 'LC_NUMERIC', 'LC_TIME', 'LC_PAPER', 'LC_NAME',
    'LC_ADDRESS', 'LC_TELEPHONE', 'LC_MEASUREMENT', 'LC_IDENTIFICATION',
    'TZ', 'TERM', 'COLORTERM', 'TERM_PROGRAM', 'TERM_PROGRAM_VERSION',
    'LINES', 'COLUMNS', 'SSH_CONNECTION', 'SSH_CLIENT', 'SSH_TTY', 'DISPLAY',
    'WAYLAND_DISPLAY', 'XDG_SESSION_TYPE', 'XDG_RUNTIME_DIR', 'XDG_DATA_DIRS',
    'XDG_CONFIG_DIRS', 'HOSTNAME', 'COMPUTERNAME', 'TMPDIR', 'TEMP', 'TMP',
    'PYTHONIOENCODING', 'PYTHONUNBUFFERED', 'PYTHONHASHSEED', 'VIRTUAL_ENV',
    'VIRTUAL_ENV_PROMPT', 'CONDA_PREFIX', 'CONDA_DEFAULT_ENV',
})

# Originals captured ONCE at guard load. Re-installing the guard must wrap the
# REAL functions — wrapping the previously-installed patched versions would
# stack closures, and a nested wrapper whose closure captured an earlier
# allowlist would keep rejecting (or accepting) paths after a sync/reset. The
# mutation helpers (`_safe_open`, `_safe_chdir`, ...) are redefined fresh each
# install, so only these module-level originals are needed.
_orig_open = builtins.open
_orig_os_open = os.open
_orig_fdopen = os.fdopen
_orig_chdir = os.chdir
_orig_scandir = os.scandir
_orig_walk = os.walk
_orig_listdir = os.listdir
_orig_path_open = pathlib.Path.open
_orig_realpath = os.path.realpath
_orig_stat = os.stat
_orig_import = builtins.__import__
_orig_path_probes = {}
for _pn in ('exists', 'isfile', 'isdir', 'lexists', 'islink', 'getsize', 'getatime', 'getmtime', 'getctime'):
    if hasattr(os.path, _pn):
        _orig_path_probes[_pn] = getattr(os.path, _pn)

def _rlm_install_guard():
    # Closure-capture the CURRENT allowlist + root at install time. The guard
    # check functions close over these locals, so a later rebinding of the
    # module globals (`_rlm_allowlist = [...]`) from model code can NEVER widen
    # the scope: the checks keep using the captured instance until the next
    # `_rlm_install_guard()` re-runs (which re-reads the synced globals and
    # closes over the fresh instance). `_rlm_install_guard` is re-run before
    # every `execute_user_code`, and `sync_kernel_allowlist`/`reset_allowlist`
    # append a re-install call after updating the globals, so approvals still
    # take effect. Combined with the private-globals rebuild in
    # `execute_user_code`, model code has no plain-globals path to the guard
    # state (only the documented `__closure__` introspection residual remains).
    _rlm_allowlist = globals().get('_rlm_allowlist', [])
    _rlm_root = globals().get('_rlm_project_root', '/')
    # Local aliases of the module-level originals (captured once at guard load,
    # so they NEVER change between installs). Every re-install therefore wraps
    # the REAL functions — never a previously-installed wrapper — and the safe
    # wrappers close over these locals, which lets `execute_user_code` rebuild
    # them against a private globals dict without losing access.
    _orig_open = globals()['_orig_open']
    _orig_os_open = globals()['_orig_os_open']
    _orig_fdopen = globals()['_orig_fdopen']
    _orig_chdir = globals()['_orig_chdir']
    _orig_scandir = globals()['_orig_scandir']
    _orig_walk = globals()['_orig_walk']
    _orig_listdir = globals()['_orig_listdir']
    _orig_path_open = globals()['_orig_path_open']
    _orig_realpath = globals()['_orig_realpath']
    _orig_stat = globals()['_orig_stat']
    _orig_import = globals()['_orig_import']
    # The `_orig_*` originals are module-level (captured once, see above); only
    # the environment snapshot is taken per-install (it is re-sanitized from the
    # previous sanitized copy, so it stays clean).
    _orig_env = dict(os.environ)

    # `_rlm_allowlist` may already be an `_rlm_Allowlist` on a re-install; wrap a
    # plain list (from the bootstrap snippet) into the immutable container once.
    if not isinstance(_rlm_allowlist, _rlm_Allowlist):
        _rlm_allowlist = _rlm_Allowlist(_rlm_allowlist)

    _rlm_denylist = (
        os.path.expanduser('~/.ssh'),
        os.path.expanduser('~/.secrets'),
        os.path.expanduser('~/.aws'),
        os.path.expanduser('~/.azure'),
        os.path.expanduser('~/.gcloud'),
        os.path.expanduser('~/.kube'),
        os.path.expanduser('~/.config'),
        os.path.expanduser('~/.gnupg'),
        os.path.expanduser('~/.docker'),
        os.path.expanduser('~/secrets'),
        os.path.expanduser('~/Library'),
        os.path.expanduser('~/AppData'),
        '/etc',
        '/var',
        '/private/etc',
    )

    # Replace the inherited (secret-bearing) environment with a sanitized copy.
    os.environ = {k: v for k, v in _orig_env.items() if k in _RLM_ENV_ALLOW}

    def _rlm_real(path):
        # `_orig_realpath` is the original posixpath.realpath. NOTE: realpath
        # internally calls os.lstat/os.readlink, which are patched below — those
        # patched probes only use the RAW checks (no realpath), so there is no
        # recursion. Out-of-scope components make a probe raise, realpath fails,
        # and this returns None (denied).
        try:
            return _orig_realpath(path)
        except Exception:
            return None

    # Raw (no symlink resolution) scope/deny checks used by the metadata-probe
    # patches. They MUST NOT call `_rlm_real`/`_rlm_in_scope`: realpath calls the
    # patched os.lstat/os.readlink, and a probe that re-enters realpath would
    # recurse forever.
    def _rlm_raw_in_scope(path):
        p = str(path)
        if p == _rlm_root or p.startswith(_rlm_root + os.sep):
            return True
        for root in _rlm_allowlist:
            if p == root or p.startswith(root + os.sep):
                return True
        return False

    def _rlm_raw_is_denied(path):
        p = str(path)
        for d in _rlm_denylist:
            if p == d or p.startswith(d + os.sep):
                return True
        if os.path.basename(p).startswith('.env'):
            return True
        return False

    def _rlm_in_scope(path):
        rp = _rlm_real(path)
        if rp is None:
            return False
        if rp == _rlm_root or rp.startswith(_rlm_root + os.sep):
            return True
        for root in _rlm_allowlist:
            if rp == root or rp.startswith(root + os.sep):
                return True
        return False

    def _rlm_is_denied(path):
        rp = _rlm_real(path)
        if rp is None:
            return False
        for d in _rlm_denylist:
            if rp == d or rp.startswith(d + os.sep):
                return True
        if os.path.basename(rp).startswith('.env'):
            return True
        return False

    def _safe_open(file, mode='r', *args, **kwargs):
        m = str(mode)
        if any(flag in m for flag in ['w', 'a', '+', 'x']):
            raise ReadOnlyError("RLM Kernel is READ-ONLY: Attempted to open '%s' with mode '%s'. File modifications are blocked." % (file, mode))
        rp = _rlm_real(file)
        if rp is None or not _rlm_in_scope(file):
            raise ReadOnlyError("RLM Kernel READ BLOCKED_EXTERNAL: '%s' is outside the project and not in the external allowlist. Use request_external_access to ask the user." % file)
        if _rlm_is_denied(file):
            raise ReadOnlyError("RLM Kernel SECURITY: '%s' is a sensitive path and cannot be read." % file)
        f = _orig_open(file, mode, *args, **kwargs)
        # Post-open verification: the opened file must be the same inode we validated,
        # guarding against a symlink swap between the scope check and the open.
        if rp is not None:
            try:
                expected = os.stat(rp)
                opened = os.fstat(f.fileno())
                if opened.st_dev != expected.st_dev or opened.st_ino != expected.st_ino:
                    f.close()
                    raise ReadOnlyError("RLM Kernel SECURITY: '%s' changed after the scope check (symlink swap?)." % file)
            except ReadOnlyError:
                raise
            except Exception:
                pass
        return f

    def _block_mutation(name):
        def _blocked(*args, **kwargs):
            raise ReadOnlyError("RLM Kernel is READ-ONLY: os.%s() is blocked to prevent modifying/deleting files." % name)
        return _blocked

    # Metadata probes are read-boundary checks too: `os.stat('~/.ssh/id_rsa')`
    # reveals file existence/size even though the content read is blocked.
    # Only `os.stat` and the `os.path.*` helpers are patched — NEVER `os.lstat`
    # or `os.readlink`, which `os.path.realpath` calls internally. Patching those
    # made realpath fail for `/var/...` (macOS maps /var -> /private/var) and
    # broke every read. Because lstat/readlink stay real, the probes can safely
    # use the FULL `_rlm_in_scope`/`_rlm_is_denied` (which realpath) — no recursion.
    def _make_safe_probe(_pname, _orig_fn):
        def _probe(path, *args, **kwargs):
            if not _rlm_in_scope(path):
                if _rlm_is_denied(path):
                    raise ReadOnlyError("RLM Kernel SECURITY: %s on sensitive path: '%s'" % (_pname, path))
                raise ReadOnlyError("RLM Kernel READ BLOCKED_EXTERNAL: %s outside scope: '%s'" % (_pname, path))
            return _orig_fn(path, *args, **kwargs)
        return _probe
    os.stat = _make_safe_probe('stat', _orig_stat)
    for _pn in ('exists', 'isfile', 'isdir', 'lexists', 'islink', 'getsize', 'getatime', 'getmtime', 'getctime'):
        if _pn in _orig_path_probes:
            setattr(os.path, _pn, _make_safe_probe(_pn, _orig_path_probes[_pn]))

    for _f in ['remove', 'unlink', 'rmdir', 'mkdir', 'makedirs', 'rename', 'renames', 'replace', 'write', 'truncate', 'chmod', 'chown', 'system',
               'kill', 'killpg', 'abort', '_exit', 'setsid', 'setuid', 'setgid', 'chroot', 'sethostname']:
        if hasattr(os, _f):
            setattr(os, _f, _block_mutation(_f))

    for _f in ['rmtree', 'move', 'copy', 'copy2', 'copyfile', 'copystat', 'copymode']:
        if hasattr(shutil, _f):
            setattr(shutil, _f, _block_mutation(_f))

    for _f in ['write_text', 'write_bytes', 'unlink', 'rmdir', 'mkdir', 'rename', 'replace', 'chmod', 'touch']:
        if hasattr(pathlib.Path, _f):
            setattr(pathlib.Path, _f, _block_mutation(_f))

    def _blocked_sub(*args, **kwargs):
        raise ReadOnlyError("RLM Kernel is READ-ONLY: Subprocess execution from inside Python is blocked.")
    subprocess.Popen = _blocked_sub
    subprocess.run = _blocked_sub
    subprocess.call = _blocked_sub

    # Block chdir / scandir / walk outside scope (read boundary).
    def _safe_chdir(path):
        if not _rlm_in_scope(path):
            raise ReadOnlyError("RLM Kernel: os.chdir outside scope is blocked: '%s'" % path)
        return _orig_chdir(path)
    os.chdir = _safe_chdir

    def _safe_scandir(path='.'):
        if not _rlm_in_scope(path):
            raise ReadOnlyError("RLM Kernel READ BLOCKED_EXTERNAL: scandir outside scope: '%s'" % path)
        return _orig_scandir(path)
    os.scandir = _safe_scandir

    def _safe_walk(top, topdown=True, onerror=None, followlinks=False):
        if not _rlm_in_scope(top):
            err = ReadOnlyError("RLM Kernel READ BLOCKED_EXTERNAL: os.walk outside scope: '%s'" % top)
            if onerror:
                onerror(err)
            return []
        return _orig_walk(top, topdown, onerror, followlinks)
    os.walk = _safe_walk

    # ---- Additional read-boundary hardening -------------------------------------
    # `io.open` is a distinct function object from the `builtins.open` name; the
    # `builtins.open` rebind above does NOT affect it, so rebind it explicitly.
    _io.open = _safe_open

    # More mutation / spawn / exec entry points on `os`.
    for _f in ['popen', 'spawnv', 'spawnve', 'spawnl', 'spawnle', 'spawnlp', 'spawnlpe',
               'spawnvp', 'spawnvpe', 'fork', 'forkpty', 'execv', 'execve', 'execvp',
               'execvpe', 'execl', 'execle', 'execlp', 'execlpe', 'startfile']:
        if hasattr(os, _f):
            setattr(os, _f, _block_mutation(_f))

    # os.open / os.fdopen: allow read-only access inside scope only.
    def _safe_os_open(path, flags, *args, **kwargs):
        _write_flags = os.O_WRONLY | os.O_RDWR | os.O_CREAT | os.O_TRUNC | os.O_APPEND | os.O_EXCL
        if flags & _write_flags:
            raise ReadOnlyError("RLM Kernel is READ-ONLY: os.open with write/truncate flags is blocked.")
        if not _rlm_in_scope(path):
            raise ReadOnlyError("RLM Kernel READ BLOCKED_EXTERNAL: os.open outside scope: '%s'" % path)
        if _rlm_is_denied(path):
            raise ReadOnlyError("RLM Kernel SECURITY: os.open denied path: '%s'" % path)
        rp = _rlm_real(path)
        fd = _orig_os_open(path, flags, *args, **kwargs)
        # Post-open inode verification guards against a symlink swap between the
        # scope check and the open.
        if rp is not None:
            try:
                expected = os.stat(rp)
                opened = os.fstat(fd)
                if opened.st_dev != expected.st_dev or opened.st_ino != expected.st_ino:
                    os.close(fd)
                    raise ReadOnlyError("RLM Kernel SECURITY: '%s' changed after the scope check (symlink swap?)." % path)
            except ReadOnlyError:
                raise
            except Exception:
                pass
        return fd
    os.open = _safe_os_open

    def _safe_fdopen(fd, mode='r', *args, **kwargs):
        _m = str(mode)
        if any(_f in _m for _f in ['w', 'a', '+', 'x']):
            raise ReadOnlyError("RLM Kernel is READ-ONLY: os.fdopen write mode blocked.")
        return _orig_fdopen(fd, mode, *args, **kwargs)
    os.fdopen = _safe_fdopen

    # os.listdir: enforce the same read scope as os.scandir/os.walk.
    # NOTE: os.readlink is intentionally NOT patched here — os.path.realpath() calls
    # it internally, and a readlink patch that re-enters _rlm_in_scope() would recurse
    # infinitely (realpath -> readlink -> realpath -> ...).
    def _safe_listdir(path='.'):
        if not _rlm_in_scope(path):
            raise ReadOnlyError("RLM Kernel READ BLOCKED_EXTERNAL: os.listdir outside scope: '%s'" % path)
        return _orig_listdir(path)
    os.listdir = _safe_listdir

    # pathlib read entry points route through the guarded open.
    def _safe_path_open(self, mode='r', *args, **kwargs):
        return _safe_open(str(self), mode, *args, **kwargs)
    pathlib.Path.open = _safe_path_open

    def _safe_path_read_text(self, *args, **kwargs):
        return _safe_path_open(self, 'r', *args, **kwargs).read()
    pathlib.Path.read_text = _safe_path_read_text

    def _safe_path_read_bytes(self, *args, **kwargs):
        return _safe_path_open(self, 'rb', *args, **kwargs).read()
    pathlib.Path.read_bytes = _safe_path_read_bytes

    if hasattr(pathlib.Path, '_raw_open'):
        pathlib.Path._raw_open = staticmethod(_safe_open)

    # Block importing memory-escape / network / low-level modules. Setting the
    # entry to None makes any later `import` raise ImportError (including via
    # importlib). `posix` is the C module that backs `os` — the guard patches
    # `os.open`/`os.remove` etc., but NOT `posix.open`/`posix.unlink`, so an
    # `import posix` would read/delete ANY file, bypassing the whole guard.
    # `gc`/`inspect` are classic escape hatches (gc.get_objects can reach the
    # guard closure cells; inspect can walk frames/modules). `sys` must be
    # blocked too: model code that runs in the restricted namespace could
    # otherwise reach the FULL session globals through
    # `sys.modules['__main__'].__dict__` and mutate `_rlm_allowlist` /
    # `_rlm_project_root`, silently bypassing the external-access gate.
    for _m in ['ctypes', 'cffi', 'socket', 'pty', 'fcntl', 'posix', '_posixsubprocess',
               'gc', 'inspect']:
        _sys.modules[_m] = None

    # ---- Sandbox hardening -------------------------------------------------------
    # `importlib.reload(os)` would re-execute the os module and restore the
    # unpatched attributes — block it.
    if hasattr(_importlib, 'reload'):
        _importlib.reload = _block_mutation('reload')

    # A fresh re-import of a patched module (after `sys.modules.pop(name)`) yields
    # an UNPATCHED module object. Route every import of a protected module back to
    # the (patched) one already in sys.modules, or refuse when it was popped.
    # `_orig_import` is the module-level original captured before the first patch.
    _protected_imports = {'os', 'io', '_io', 'shutil', 'pathlib', 'subprocess',
                          'importlib', 'sys', 'gc', 'inspect', 'codeop', 'code',
                          'posix', '_posixsubprocess'}
    def _safe_import(name, *args, **kwargs):
        top = name.split('.')[0]
        # `builtins` hands out every patched function whose closure leaks the
        # saved originals — refuse it entirely. Everything else in the protected
        # set is routed back to the (patched) module already in sys.modules.
        if top == 'builtins':
            raise ImportError("import of 'builtins' is restricted in the RLM kernel")
        if top in _protected_imports:
            mod = _sys.modules.get(top)
            if mod is None:
                raise ImportError("import of '%s' is restricted in the RLM kernel" % name)
            return mod
        return _orig_import(name, *args, **kwargs)
    builtins.__import__ = _safe_import

    builtins.open = _safe_open

    # `exit()`/`quit()` (site helpers) raise SystemExit and would terminate the
    # persistent kernel process mid-session.
    for _b in ('exit', 'quit'):
        if hasattr(builtins, _b):
            setattr(builtins, _b, _block_mutation(_b))

    # Export only the whitelisted helpers the memo/helper snippets rely on; the
    # saved originals stay hidden in this function's closure. The function is
    # KEPT (not deleted) so `execute_user_code` can re-run it before every exec
    # and roll back any module/builtins mutation a previous snippet performed.
    # Re-publish the fresh check functions under the module-level names: the
    # guard now captures the allowlist/root in its closure, so a re-install that
    # only rebuilt local functions would leave the global `_rlm_in_scope` /
    # `_rlm_is_denied` pointing at the OLD closure (and approvals from
    # `sync_kernel_allowlist` / `reset_allowlist` would never take effect).
    globals()['_rlm_in_scope'] = _rlm_in_scope
    globals()['_rlm_is_denied'] = _rlm_is_denied
    return (_rlm_in_scope, _rlm_is_denied)

(_rlm_in_scope, _rlm_is_denied) = _rlm_install_guard()
"#;

/// Memoization helper: `_rlm_load(path)` reads a file with mtime+size+sha256
/// invalidation, caching content in `_rlm_index` so unchanged files are not
/// re-read across RLM Model calls. Scope is enforced via the guarded `open`.
/// Fast path trusts `(mtime, size)`; any mismatch re-reads and falls back to a
/// sha256 comparison so timestamp-preserving writes (`cp -p`, `rsync --times`)
/// never serve stale content.
const MEMO_PY: &str = r#"
import hashlib, time
_rlm_index = {}

def _rlm_load(path):
    if not _rlm_in_scope(path):
        raise ReadOnlyError("RLM Kernel READ BLOCKED_EXTERNAL: _rlm_load outside scope: '%s'" % path)
    if _rlm_is_denied(path):
        raise ReadOnlyError("RLM Kernel SECURITY: _rlm_load denied path: '%s'" % path)
    key = os.path.realpath(path)
    st = os.stat(path)
    entry = _rlm_index.get(key)
    if entry is not None and entry['mtime'] == st.st_mtime and entry['size'] == st.st_size:
        return entry['content']
    with open(path, 'r') as f:
        content = f.read()
    sha = hashlib.sha256(content.encode('utf-8', errors='replace')).hexdigest()
    if entry is not None and entry['sha'] == sha:
        # Content unchanged but mtime/size drifted (cp -p / touch -r): refresh
        # metadata and serve the cached content instead of re-processing it.
        entry['mtime'] = st.st_mtime
        entry['size'] = st.st_size
        return entry['content']
    _rlm_index[key] = {'mtime': st.st_mtime, 'size': st.st_size, 'sha': sha, 'content': content, 'loaded_at': time.time()}
    return content

def _rlm_forget(path=None):
    if path is None:
        _rlm_index.clear()
    else:
        _rlm_index.pop(os.path.realpath(path), None)
"#;

/// Compact read helpers: `_rlm_symbols`, `_rlm_grep`, `_rlm_snippet`. All three
/// respect the read-only scope guard (BLOCKED_EXTERNAL / SECURITY) and cap their
/// own output so a careless call can never dump a whole file into the context.
const HELPER_PY: &str = r#"
import fnmatch as _fnmatch, re as _re

_MAX_HELPER_CHARS = 24000
_MAX_GREP_MATCHES = 100
_MAX_GREP_FILE_BYTES = 1_000_000
_HELPER_SKIP_DIRS = {
    'node_modules', 'target', 'dist', 'build', '.git', '.svn', '.hg',
    'venv', '.venv', '__pycache__', '.kuda', '.idea', '.vscode', '.next',
    '.turbo', '.cache', '.cargo', 'Pods', 'DerivedData',
}

def _rlm_cap(text, limit=_MAX_HELPER_CHARS):
    if text is None:
        return ''
    if len(text) <= limit:
        return text
    return text[:limit] + '\n... [TRUNCATED: %d more chars omitted]' % (len(text) - limit)

def _rlm_rel(path):
    try:
        r = os.path.relpath(path, _rlm_project_root)
        if not r.startswith('..'):
            return r
    except Exception:
        pass
    return str(path)

def _rlm_symbols(path):
    if not _rlm_in_scope(path):
        raise ReadOnlyError("RLM Kernel READ BLOCKED_EXTERNAL: _rlm_symbols outside scope: '%s'" % path)
    if _rlm_is_denied(path):
        raise ReadOnlyError("RLM Kernel SECURITY: _rlm_symbols denied path: '%s'" % path)
    content = _rlm_load(path)
    line_re = _re.compile(
        r'^\s*(?:(?:async\s+|export\s+(?:default\s+)?)?(?:def|function|class)|'
        r'(?:pub\s+)?(?:fn|struct|enum|trait|impl)|'
        r'func(?:\s*\([^)]*\)\s*)?)\s+([A-Za-z_$][\w$]*)'
    )
    const_re = _re.compile(
        r'^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*(?:function|async|\(|class)'
    )
    type_re = _re.compile(r'^\s*(?:export\s+)?(?:interface|type)\s+([A-Za-z_$][\w$]*)')
    out = []
    for i, ln in enumerate(content.splitlines(), 1):
        if line_re.match(ln) or const_re.match(ln) or type_re.match(ln):
            out.append('%s:%d:%s' % (_rlm_rel(path), i, ln.strip()))
    if not out:
        return '(no definitions matched in %s)' % _rlm_rel(path)
    return _rlm_cap('\n'.join(out))

def _rlm_grep(pattern, dir='.'):
    if not _rlm_in_scope(dir):
        raise ReadOnlyError("RLM Kernel READ BLOCKED_EXTERNAL: _rlm_grep outside scope: '%s'" % dir)
    if _rlm_is_denied(dir):
        raise ReadOnlyError("RLM Kernel SECURITY: _rlm_grep denied path: '%s'" % dir)
    try:
        pat = _re.compile(pattern)
    except _re.error as e:
        return '(invalid regex: %s)' % e
    matches = []
    for root, dirs, files in os.walk(dir):
        dirs[:] = [d for d in dirs if d not in _HELPER_SKIP_DIRS and not d.startswith('.')]
        for fn in files:
            if len(matches) >= _MAX_GREP_MATCHES:
                break
            if fn.startswith('.') or fn in _HELPER_SKIP_DIRS:
                continue
            fp = os.path.join(root, fn)
            try:
                if os.path.getsize(fp) > _MAX_GREP_FILE_BYTES:
                    continue
                with open(fp, 'r', errors='replace') as f:
                    for lineno, line in enumerate(f, 1):
                        if pat.search(line):
                            matches.append('%s:%d:%s' % (_rlm_rel(fp), lineno, line.rstrip('\n').strip()))
                            if len(matches) >= _MAX_GREP_MATCHES:
                                break
            except Exception:
                continue
        if len(matches) >= _MAX_GREP_MATCHES:
            break
    if not matches:
        return '(no matches for %r under %s)' % (pattern, _rlm_rel(dir))
    body = '\n'.join(matches)
    if len(matches) >= _MAX_GREP_MATCHES:
        body += '\n... [grep hit the %d-match cap]' % _MAX_GREP_MATCHES
    return _rlm_cap(body)

def _rlm_snippet(path, start, end):
    if not _rlm_in_scope(path):
        raise ReadOnlyError("RLM Kernel READ BLOCKED_EXTERNAL: _rlm_snippet outside scope: '%s'" % path)
    if _rlm_is_denied(path):
        raise ReadOnlyError("RLM Kernel SECURITY: _rlm_snippet denied path: '%s'" % path)
    try:
        start = int(start)
        end = int(end)
    except (TypeError, ValueError):
        return '(start/end must be integers)'
    if start < 1 or end < start:
        return '(invalid range: need 1 <= start <= end, got %s..%s)' % (start, end)
    lines = []
    with open(path, 'r', errors='replace') as f:
        for lineno, line in enumerate(f, 1):
            if lineno > end:
                break
            if lineno >= start:
                lines.append('%d:%s' % (lineno, line.rstrip('\n')))
    if not lines:
        return '(no lines in range %d..%d)' % (start, end)
    return _rlm_cap('%s [%d-%d]\n%s' % (_rlm_rel(path), start, end, '\n'.join(lines)))

# ---- Snippet bank: capture exact regions ONCE, reference by id in the brief.
# The RLM Model must NOT retype code into the brief (transcription is lossy).
# It captures regions here (kernel keeps the exact bytes) and writes
# `[SNIPPET id="N"]` placeholders; the swarm expands them into verbatim
# `--- path [start-end]` blocks before the brief reaches the Thinker.
_rlm_snippet_seq = [0]
_rlm_bank = {}

def _rlm_capture(path, start=1, end=None, label=''):
    if not _rlm_in_scope(path):
        raise ReadOnlyError("RLM Kernel READ BLOCKED_EXTERNAL: _rlm_capture outside scope: '%s'" % path)
    if _rlm_is_denied(path):
        raise ReadOnlyError("RLM Kernel SECURITY: _rlm_capture denied path: '%s'" % path)
    try:
        start = int(start)
    except (TypeError, ValueError):
        return '(start must be an integer)'
    if start < 1:
        return '(start must be >= 1)'
    if end is not None:
        try:
            end = int(end)
        except (TypeError, ValueError):
            return '(end must be an integer)'
    lines = []
    with open(path, 'r', errors='replace') as f:
        for lineno, line in enumerate(f, 1):
            if end is not None and lineno > end:
                break
            if lineno >= start:
                lines.append(line.rstrip('\n'))
    if not lines:
        return '(no lines at %s starting %d)' % (_rlm_rel(path), start)
    content = '\n'.join(lines)
    eff_end = end if end is not None else start + len(lines) - 1
    _rlm_snippet_seq[0] += 1
    sid = _rlm_snippet_seq[0]
    _rlm_bank[sid] = {'rel': _rlm_rel(path), 'path': os.path.realpath(path),
                      'start': start, 'end': eff_end, 'content': content}
    return 'CAPTURED id=%d %s [%d-%d] (%d chars)%s' % (
        sid, _rlm_rel(path), start, eff_end, len(content),
        (' — ' + label) if label else '')

def _rlm_snippets():
    if not _rlm_bank:
        return '(no snippets captured yet — call _rlm_capture(path, start, end, label))'
    out = []
    for sid in sorted(_rlm_bank):
        s = _rlm_bank[sid]
        out.append('id=%d %s [%d-%d] (%d chars)' % (sid, s['rel'], s['start'], s['end'], len(s['content'])))
    return '\n'.join(out)

def _rlm_snippet_get(sid):
    try:
        sid = int(sid)
    except (TypeError, ValueError):
        return '(id must be an integer)'
    s = _rlm_bank.get(sid)
    if s is None:
        return '(no captured snippet with id=%d — check _rlm_snippets())' % sid
    return s['content']

def _rlm_snippet_block(sid):
    try:
        sid = int(sid)
    except (TypeError, ValueError):
        return ''
    s = _rlm_bank.get(sid)
    if s is None:
        return ''
    return '--- %s [%d-%d]\n%s' % (s['rel'], s['start'], s['end'], s['content'])
"#;

/// Scratch dir where staged command files live. It is injected into the kernel's
/// `_rlm_allowlist` so the read-only guard permits reading it back for `exec`.
/// Created with mode 0700 so other local users cannot read staged snippets
/// (which may contain project source) before they are removed.
fn rlm_scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("kuda_rlm_scratch");
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    dir
}

/// Writes `code` to a 0600 scratch file staged for `exec` (never world-readable,
/// even briefly: the mode is set at creation, not after the write).
fn stage_scratch_file(uuid: &str, code: &str) -> std::io::Result<PathBuf> {
    let file_path = rlm_scratch_dir().join(format!("cmd_{}.py", uuid));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&file_path)?;
        f.write_all(code.as_bytes())?;
        Ok(file_path)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&file_path, code)?;
        Ok(file_path)
    }
}

/// Composes the bootstrap Python snippet that injects `_rlm_project_root` and
/// `_rlm_allowlist` (base64-encoded so paths can't break out of the literal).
fn compose_kernel_bootstrap(project_root: &Path, allowlist: &[PathBuf]) -> String {
    let mut s = String::new();
    s.push_str("import builtins, os, shutil, pathlib, subprocess, base64 as _b64\n");
    s.push_str("_rlm_project_root = os.path.realpath(_b64.b64decode(b'");
    s.push_str(&BASE64.encode(project_root.to_string_lossy().as_bytes()));
    s.push_str("').decode())\n");
    // Plain list here: `_rlm_Allowlist` (defined by the guard script that runs
    // AFTER this bootstrap) wraps it into the immutable container at install.
    s.push_str("_rlm_allowlist = [");
    for (i, root) in allowlist.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str("os.path.realpath(_b64.b64decode(b'");
        s.push_str(&BASE64.encode(root.to_string_lossy().as_bytes()));
        s.push_str("').decode())");
    }
    s.push_str("]\n");
    s
}

pub struct RlmKernelProcess {
    pub child: Child,
    pub stdin: ChildStdin,
    pub stdout_reader: tokio::io::Lines<BufReader<ChildStdout>>,
    pub stderr_lines: Arc<Mutex<Vec<String>>>,
    pub project_root: PathBuf,
}

impl RlmKernelProcess {
    pub async fn spawn(project_root: &Path, allowlist: &[PathBuf]) -> Result<Self> {
        let mut cmd = tokio::process::Command::new("python3");
        cmd.args(["-u", "-i", "-q"])
            .current_dir(project_root)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| {
            AppError::General(format!(
                "Python 3 process failed to launch: {}. Ensure 'python3' is installed and in PATH.",
                e
            ))
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AppError::General("Failed to capture Python stdin pipe".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::General("Failed to capture Python stdout pipe".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AppError::General("Failed to capture Python stderr pipe".to_string()))?;

        let stderr_lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let stderr_capture = stderr_lines.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                stderr_capture.lock().await.push(line);
            }
        });

        let stdout_reader = BufReader::new(stdout).lines();

        let mut proc = Self {
            child,
            stdin,
            stdout_reader,
            stderr_lines,
            project_root: project_root.to_path_buf(),
        };

        // Inject bootstrap + Read-Only Security Guard + Memo helper into the session.
        // The scratch dir is allowlisted so staged command files can be read back.
        let scratch = rlm_scratch_dir();
        let mut effective_allowlist: Vec<PathBuf> = vec![scratch];
        effective_allowlist.extend(allowlist.iter().cloned());

        let init_code = format!(
            "{}\n{}\n{}\n{}",
            compose_kernel_bootstrap(project_root, &effective_allowlist),
            READONLY_GUARD_PY,
            MEMO_PY,
            HELPER_PY
        );
        proc.execute_code(&init_code, 10).await.map_err(|e| {
            AppError::General(format!("Failed to initialize RLM Read-Only Security Guard: {}", e))
        })?;

        Ok(proc)
    }

    pub async fn execute_code(&mut self, code: &str, timeout_secs: u64) -> Result<String> {
        let (file_path, path_b64, sentinel) = self.stage_code(code)?;
        let py_cmd = format!(
            "import base64 as _rlm_b64\n_rlm_p = _rlm_b64.b64decode(b'{}').decode()\nexec(open(_rlm_p).read(), globals())\nprint(\"{}\")\n",
            path_b64, sentinel
        );
        self.exec_staged(&py_cmd, &sentinel, &file_path, timeout_secs).await
    }

    /// Executes MODEL-supplied code in a RESTRICTED namespace. Only the public
    /// RLM helper functions (and a sanitized builtins COPY) are visible, so model
    /// code can NEVER reach the sandbox's own state: `_rlm_allowlist`,
    /// `_rlm_project_root`, `_rlm_in_scope`'s closure, `sys`, `builtins`, etc.
    /// This closes the "model appends to `_rlm_allowlist` to self-approve broad
    /// external reads" bypass — the mutation would only touch the throwaway
    /// per-exec namespace, never the real guard state.
    ///
    /// The guard is re-installed BEFORE the snippet runs so any persistent
    /// mutation (e.g. `os.environ`, a re-patched `builtins.open`, a widened
    /// allowlist) made by a PREVIOUS snippet is rolled back first. `__builtins__`
    /// is a per-execution dict copy with introspection/process-exit primitives
    /// stripped (`open`, `getattr`, `globals`, `eval`, `exit`, ...), so
    /// `__builtins__['open'] = ...` can never poison the shared module.
    ///
    /// The exposed helper functions are REBUILT (`types.FunctionType`) against a
    /// PRIVATE globals dict that contains only the names they need — never the
    /// session namespace. Because `_rlm_load.__globals__` therefore has no
    /// `_rlm_allowlist` / `_rlm_project_root` / `builtins` key, the classic
    /// `_rlm_load.__globals__['_rlm_allowlist'] = ['']` sandbox escape (which
    /// used to widen the read scope in one step) now fails with a KeyError. The
    /// guard functions themselves are also rebuilt against the private dict and
    /// read the allowlist/root from their closure (see `_rlm_install_guard`), so
    /// even a leaked reference cannot rebind what the checks consult.
    pub async fn execute_user_code(&mut self, code: &str, timeout_secs: u64) -> Result<String> {
        let (file_path, path_b64, sentinel) = self.stage_code(code)?;
        let user_names = RLM_USER_NAMESPACE_NAMES
            .iter()
            .map(|n| format!("'{}'", n))
            .collect::<Vec<_>>()
            .join(", ");
        // Globals the rebuilt helpers resolve against. Everything here is either
        // guarded (`os`, `open`), a plain value (`_rlm_project_root`), or shared
        // memo state (`_rlm_index` / `_rlm_bank` / `_rlm_snippet_seq`). The
        // allowlist itself is deliberately NOT included.
        let private_names = [
            "os",
            "time",
            "hashlib",
            "_re",
            "_fnmatch",
            "ReadOnlyError",
            "_rlm_project_root",
            "_rlm_index",
            "_rlm_bank",
            "_rlm_snippet_seq",
            "_MAX_HELPER_CHARS",
            "_MAX_GREP_MATCHES",
            "_MAX_GREP_FILE_BYTES",
            "_HELPER_SKIP_DIRS",
        ]
        .iter()
        .map(|n| format!("'{}'", n))
        .collect::<Vec<_>>()
        .join(", ");
        let py_cmd = format!(
            "import base64 as _rlm_b64, types as _rlm_ty\n\
             _rlm_install_guard()\n\
             _rlm_p = _rlm_b64.b64decode(b'{}').decode()\n\
             _rlm_b = globals().get('__builtins__')\n\
             _rlm_bd = getattr(_rlm_b, '__dict__', _rlm_b) if _rlm_b is not None else {{}}\n\
             _rlm_bs = {{k: v for k, v in _rlm_bd.items() if k not in _RLM_BLOCKED_BUILTINS}}\n\
             _rlm_priv = {{n: globals()[n] for n in [{}]}}\n\
             _rlm_priv['open'] = _rlm_ty.FunctionType(builtins.open.__code__, _rlm_priv, '_safe_open', builtins.open.__defaults__, builtins.open.__closure__)\n\
             _rlm_priv['__builtins__'] = _rlm_bs\n\
             _rlm_priv['_rlm_in_scope'] = _rlm_ty.FunctionType(globals()['_rlm_in_scope'].__code__, _rlm_priv, '_rlm_in_scope', globals()['_rlm_in_scope'].__defaults__, globals()['_rlm_in_scope'].__closure__)\n\
             _rlm_priv['_rlm_is_denied'] = _rlm_ty.FunctionType(globals()['_rlm_is_denied'].__code__, _rlm_priv, '_rlm_is_denied', globals()['_rlm_is_denied'].__defaults__, globals()['_rlm_is_denied'].__closure__)\n\
             _rlm_priv['_rlm_cap'] = _rlm_ty.FunctionType(globals()['_rlm_cap'].__code__, _rlm_priv, '_rlm_cap', globals()['_rlm_cap'].__defaults__, globals()['_rlm_cap'].__closure__)\n\
             _rlm_user = {{n: _rlm_ty.FunctionType(globals()[n].__code__, _rlm_priv, n, globals()[n].__defaults__, globals()[n].__closure__) for n in [{}]}}\n\
             _rlm_user['__builtins__'] = _rlm_bs\n\
             _rlm_priv.update(_rlm_user)\n\
             exec(open(_rlm_p).read(), _rlm_user)\nprint(\"{}\")\n",
            path_b64, private_names, user_names, sentinel
        );
        self.exec_staged(&py_cmd, &sentinel, &file_path, timeout_secs).await
    }

    fn stage_code(&self, code: &str) -> Result<(PathBuf, String, String)> {
        let uuid = Uuid::new_v4().to_string();
        let sentinel = format!("---RLM_SENTINEL_{}---", uuid);

        // Stage the code in a scratch file and exec it via a SHORT stdin command.
        // Python's interactive line reader truncates long single-line commands
        // (>= a few KB), which hangs the REPL; keeping every stdin line tiny
        // avoids that entirely on all supported Python versions.
        let file_path = match stage_scratch_file(&uuid, code) {
            Ok(p) => p,
            Err(e) => {
                return Err(AppError::General(format!(
                    "Failed to stage Python code for execution: {}",
                    e
                )))
            }
        };
        let path_b64 = BASE64.encode(file_path.to_string_lossy().as_bytes());
        Ok((file_path, path_b64, sentinel))
    }

    async fn exec_staged(
        &mut self,
        py_cmd: &str,
        sentinel: &str,
        file_path: &std::path::Path,
        timeout_secs: u64,
    ) -> Result<String> {
        // Drop stderr leftovers from a previous run before opening this window.
        self.stderr_lines.lock().await.clear();

        self.stdin
            .write_all(py_cmd.as_bytes())
            .await
            .map_err(|e| AppError::General(format!("Failed to write to Python stdin: {}", e)))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| AppError::General(format!("Failed to flush Python stdin: {}", e)))?;

        let mut output_lines: Vec<String> = Vec::new();
        let mut found_sentinel = false;

        let read_future = async {
            while let Ok(Some(line)) = self.stdout_reader.next_line().await {
                if line.contains(&sentinel) {
                    found_sentinel = true;
                    break;
                }
                // `python -i` echoes `>>> ` / `... ` prompts to stdout between
                // statements; they are merged into the captured lines (a prompt
                // has no trailing newline, so `read_line` joins it with the next
                // output line). Strip the prompts so model output stays clean.
                let mut cleaned: &str = line.trim_start();
                while cleaned.starts_with(">>> ") || cleaned.starts_with("... ") {
                    cleaned = cleaned[4..].trim_start();
                }
                if cleaned.trim().is_empty() {
                    continue;
                }
                output_lines.push(cleaned.to_string());
            }
        };

        let result = match tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            read_future,
        )
        .await
        {
            Ok(_) => {
                if found_sentinel {
                    Ok(())
                } else {
                    Err(AppError::General(
                        "Python process terminated unexpectedly before sentinel was received.".to_string(),
                    ))
                }
            }
            Err(_) => {
                // The Python process is still running a runaway/infinite loop.
                // Kill it NOW so it cannot keep consuming the stdout pipe and
                // poison every subsequent `execute_code` call; the next
                // `get_or_spawn` sees the dead child and respawns cleanly.
                let _ = self.child.kill().await;
                let _ = self.child.wait().await;
                Err(AppError::General(format!(
                    "Python code execution timed out after {} seconds.",
                    timeout_secs
                )))
            }
        };
        let _ = std::fs::remove_file(file_path);
        result?;

        // Grace window so stderr tracebacks flushed before the sentinel are captured.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let stderr_buf = std::mem::take(&mut *self.stderr_lines.lock().await);

        let mut output = output_lines.join("\n");
        if !stderr_buf.is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str("=== STDERR ===\n");
            output.push_str(&stderr_buf.join("\n"));
        }
        Ok(output)
    }

    pub async fn kill(&mut self) {
        let _ = self.child.kill().await;
    }
}

/// Names copied into the restricted namespace used by `execute_user_code`.
/// These are the ONLY session names the model can reference; everything else
/// (guard state, allowlist, project root, sys/builtins) stays unreachable.
/// The guard functions (`_rlm_in_scope`/`_rlm_is_denied`) are intentionally
/// NOT exposed: model code goes through the read helpers, and a direct handle
/// would give `.__globals__` access to the session namespace.
const RLM_USER_NAMESPACE_NAMES: [&str; 8] = [
    "_rlm_load",
    "_rlm_grep",
    "_rlm_symbols",
    "_rlm_snippet",
    "_rlm_snippet_get",
    "_rlm_snippets",
    "_rlm_capture",
    "_rlm_rel",
];

/// Manages the persistent RLM Python kernel plus the live external-access allowlist.
///
/// The allowlist is shared (as an `Arc<Mutex<Vec<PathBuf>>>`) with `AppState` so
/// approval commands can append paths and the kernel guard picks them up either
/// live (via `add_allowed_root` executing a small snippet) or at next respawn.
pub struct RlmKernelManager {
    kernel: Arc<Mutex<Option<RlmKernelProcess>>>,
    allowlist: Arc<Mutex<Vec<PathBuf>>>,
    /// Set when the approved set changed; `get_or_spawn` re-syncs the running
    /// kernel's `_rlm_allowlist` once before returning, so an approval always
    /// takes effect even if an earlier live-update round-trip was missed.
    allowlist_dirty: AtomicBool,
}

/// Canonicalizes `path` while tolerating a missing final component (mirrors
/// Python's `os.path.realpath`): the longest existing ancestor is canonicalized
/// and the remaining components are appended. `Path::canonicalize` alone fails
/// for paths that do not exist, which silently dropped approvals for files that
/// are absent on disk (e.g. a config a build has not generated yet), leaving the
/// kernel to keep reporting "BLOCKED_EXTERNAL" instead of a real file-not-found.
fn canonicalize_lenient(path: &Path) -> PathBuf {
    if let Ok(canon) = path.canonicalize() {
        return canon;
    }
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path;
    loop {
        if let Ok(canon) = cur.canonicalize() {
            let mut out = canon;
            for comp in missing.iter().rev() {
                out.push(comp);
            }
            return out;
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) => {
                missing.push(name.to_os_string());
                cur = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Textual `Path` equality is broken for symlinked roots (macOS `/var` vs
/// `/private/var`): the kernel would respawn + clear the allowlist on every
/// call. Compare canonical forms, falling back to the raw strings when a side
/// cannot be canonicalized.
fn paths_equal(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

/// System-wide directory ROOTS that must never enter the external allowlist:
/// approving one of these (rather than a narrow subtree) would let the
/// read-only kernel enumerate/read effectively the whole machine. Matching is
/// EXACT — specific subtrees (an installed dependency dir, a sibling repo)
/// stay approvable.
const BROAD_SYSTEM_ROOTS: &[&str] = &[
    "/etc", "/var", "/usr", "/bin", "/sbin", "/lib", "/lib64", "/opt", "/boot", "/dev",
    "/sys", "/proc", "/run", "/srv", "/private", "/private/etc", "/private/tmp",
    "/private/var", "/private/opt", "/System", "/Library", "/Applications", "/Users",
    "/home", "/tmp", "/Windows", "/Program Files",
];

/// True when `canon` (already canonicalized) is a dangerously broad root that
/// must not be approved: the filesystem root, a top-level system directory,
/// or the user's home directory. The allowlist exists for narrow grants (a
/// config file, an installed dependency dir, a sibling repo) — a broad grant
/// defeats the whole read scope. Public so the IDE filesystem commands apply
/// the same guard to out-of-project approvals.
pub fn is_broad_root(canon: &Path) -> bool {
    // Filesystem root (no parent).
    if canon.parent().is_none() {
        return true;
    }
    let s = canon.to_string_lossy();
    if BROAD_SYSTEM_ROOTS.iter().any(|root| s == *root) {
        return true;
    }
    // The user's home directory itself (approving ~ exposes every dotfile
    // config, keychain material and browser profile not caught by the
    // kernel denylist).
    if let Some(home) = std::env::var_os("HOME") {
        let home_canon = canonicalize_lenient(Path::new(&home));
        if canon == &home_canon {
            return true;
        }
    }
    false
}

impl RlmKernelManager {
    pub fn new() -> Self {
        Self {
            kernel: Arc::new(Mutex::new(None)),
            allowlist: Arc::new(Mutex::new(Vec::new())),
            allowlist_dirty: AtomicBool::new(false),
        }
    }

    /// Returns a shared handle to the live allowlist so `AppState` (and approval
    /// commands) can read/mutate the same set the kernel guard uses.
    pub fn allowlist_handle(&self) -> Arc<Mutex<Vec<PathBuf>>> {
        self.allowlist.clone()
    }

    /// Returns a snapshot of the currently-allowed external roots.
    pub async fn allowlist_snapshot(&self) -> Vec<PathBuf> {
        self.allowlist.lock().await.clone()
    }

    /// Appends an external root to the allowlist. Canonicalizes the path
    /// leniently (works for paths that do not exist yet) and marks the list
    /// dirty so the running kernel re-syncs on next use. The approval is
    /// project-scoped and persists across kernel respawns.
    ///
    /// BROAD ROOTS ARE REJECTED: `/`, the user's home directory, home's
    /// ancestors and system trees (`/etc`, `/usr`, ...) would expose the whole
    /// filesystem to the read-only kernel, so they never enter the allowlist —
    /// regardless of who asked or how convincing the reason sounded.
    pub async fn add_allowed_root(&self, root: &Path) -> Result<()> {
        let canon = canonicalize_lenient(root);
        if is_broad_root(&canon) {
            return Err(AppError::General(format!(
                "Refusing to allow overly broad external path '{}': it would expose the whole \
                 filesystem to the read-only kernel. Approve a narrower path instead (a specific \
                 file or dependency directory).",
                canon.display()
            )));
        }
        {
            let mut al = self.allowlist.lock().await;
            if !al.iter().any(|p| p == &canon) {
                al.push(canon);
                self.allowlist_dirty.store(true, Ordering::SeqCst);
            }
        }
        Ok(())
    }

    /// Clears the allowlist and live-clears the running kernel's `_rlm_allowlist`.
    pub async fn reset_allowlist(&self) -> Result<()> {
        {
            self.allowlist.lock().await.clear();
            self.allowlist_dirty.store(true, Ordering::SeqCst);
        }
        let mut guard = self.kernel.lock().await;
        if let Some(proc) = guard.as_mut() {
            // The scratch dir must stay allowlisted (staged snippets are read
            // back through the guarded `open`); clear everything else.
            let scratch_b64 = BASE64.encode(rlm_scratch_dir().to_string_lossy().as_bytes());
            if let Err(e) = proc
                .execute_code(
                    &format!(
                        "import base64 as _b64, os as _os\n_rlm_allowlist = _rlm_Allowlist([_os.path.realpath(_b64.b64decode(b'{}').decode())])\n_rlm_install_guard()",
                        scratch_b64
                    ),
                    5,
                )
                .await
            {
                tracing::warn!("Failed to live-clear RLM kernel allowlist: {}", e);
            }
        }
        Ok(())
    }

    /// Replaces the running kernel's `_rlm_allowlist` with the scratch dir plus
    /// the currently approved roots. The scratch dir must always stay allowlisted
    /// because `execute_code` stages snippets there and reads them back through
    /// the guarded `open`. Idempotent, so it is safe to run repeatedly.
    async fn sync_kernel_allowlist(&self, proc: &mut RlmKernelProcess) {
        let entries = self.allowlist.lock().await.clone();
        let scratch = rlm_scratch_dir();
        let mut code = String::from("import base64 as _b64, os as _os\n_rlm_allowlist = _rlm_Allowlist([\n");
        code.push_str(&format!(
            "  _os.path.realpath(_b64.b64decode(b'{}').decode()),\n",
            BASE64.encode(scratch.to_string_lossy().as_bytes())
        ));
        for e in &entries {
            code.push_str(&format!(
                "  _os.path.realpath(_b64.b64decode(b'{}').decode()),\n",
                BASE64.encode(e.to_string_lossy().as_bytes())
            ));
        }
        code.push_str("])\n");
        code.push_str("_rlm_install_guard()\n");
        if let Err(e) = proc.execute_code(&code, 5).await {
            tracing::warn!("Failed to sync RLM kernel allowlist: {}", e);
        }
    }

    /// Pre-warms the kernel by `_rlm_load`-ing known-good project files so the
    /// RLM Model does not have to re-read them. Bounded (first
    /// `MAX_PREWARM_FILES`, best-effort) so a huge inventory never blocks a run.
    pub async fn prewarm(&self, project_root: &Path, paths: &[String]) -> Result<usize> {
        const MAX_PREWARM_FILES: usize = 50;
        if paths.is_empty() {
            return Ok(0);
        }
        let mut code = String::from("import base64 as _b\n_loaded = 0\nfor _p in [\n");
        for p in paths.iter().take(MAX_PREWARM_FILES) {
            let b64 = BASE64.encode(p.as_bytes());
            code.push_str(&format!("_b.b64decode(b'{}').decode(),\n", b64));
        }
        code.push_str(
            "]:\n  try:\n    _rlm_load(_p)\n    _loaded += 1\n  except Exception:\n    pass\nprint('PREWARM_LOADED:' + str(_loaded))\n",
        );
        let mut guard = self.get_or_spawn(project_root).await?;
        let Some(proc) = guard.as_mut() else {
            return Ok(0);
        };
        let out = match proc.execute_code(&code, 20).await {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("RLM kernel prewarm failed: {}", e);
                return Ok(0);
            }
        };
        Ok(out
            .lines()
            .find_map(|l| l.trim().strip_prefix("PREWARM_LOADED:"))
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(0))
    }

    /// Snapshots the files currently held in the kernel's `_rlm_index`, relative
    /// to the project root, so a later session can pre-warm a fresh kernel.
    pub async fn inventory_snapshot(&self, project_root: &Path) -> KernelInventory {
        let loaded_paths = {
            let mut guard = match self.get_or_spawn(project_root).await {
                Ok(g) => g,
                Err(_) => return KernelInventory {
                    loaded_paths: Vec::new(),
                    generated_at: chrono::Local::now(),
                },
            };
            let Some(proc) = guard.as_mut() else {
                return KernelInventory {
                    loaded_paths: Vec::new(),
                    generated_at: chrono::Local::now(),
                };
            };
            let out = match proc
                .execute_code(
                    "import json\nprint('RLM_INDEX_KEYS:' + json.dumps(sorted(_rlm_index.keys())))",
                    10,
                )
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    tracing::warn!("RLM inventory snapshot failed: {}", e);
                    String::new()
                }
            };
            out.lines()
                .find_map(|l| l.trim().strip_prefix("RLM_INDEX_KEYS:"))
                .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
                .unwrap_or_default()
        };

        // Convert absolute kernel keys to project-relative paths.
        let root_norm = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());
        let rel: Vec<String> = loaded_paths
            .iter()
            .filter_map(|p| {
                let pb = PathBuf::from(p);
                if let Ok(r) = pb.strip_prefix(&root_norm) {
                    let s = r.to_string_lossy().to_string();
                    if !s.is_empty() {
                        return Some(s);
                    }
                }
                None
            })
            .collect();
        KernelInventory {
            loaded_paths: rel,
            generated_at: chrono::Local::now(),
        }
    }

    pub async fn get_or_spawn(
        &self,
        project_root: &Path,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<RlmKernelProcess>>> {
        // Approvals are project-scoped: a new project must not inherit the
        // previous project's external allowlist.
        let project_changed = {
            let guard = self.kernel.lock().await;
            match guard.as_ref() {
                Some(proc) => !paths_equal(&proc.project_root, project_root),
                None => false,
            }
        };
        if project_changed {
            self.allowlist.lock().await.clear();
            self.allowlist_dirty.store(true, Ordering::SeqCst);
        }

        let allowlist = self.allowlist.lock().await.clone();
        let mut guard = self.kernel.lock().await;

        let mut spawned = false;
        let needs_respawn = match guard.as_mut() {
            Some(proc) => {
                // Respawn if the project changed OR the previous process died
                // (e.g. was killed by a test/approval flow without a respawn).
                let dead = proc.child.try_wait().ok().flatten().is_some();
                !paths_equal(&proc.project_root, project_root) || dead
            }
            None => true,
        };

        if needs_respawn {
            if let Some(mut old_proc) = guard.take() {
                old_proc.kill().await;
            }
            let new_proc = RlmKernelProcess::spawn(project_root, &allowlist).await?;
            *guard = Some(new_proc);
            spawned = true;
        }

        // An approval happened since the last time the kernel was consulted:
        // re-sync its `_rlm_allowlist` so the granted paths take effect even if
        // a live-update round-trip was missed. A fresh spawn already received the
        // current allowlist at bootstrap, so skip the redundant round-trip.
        if !spawned && self.allowlist_dirty.swap(false, Ordering::SeqCst) {
            if let Some(proc) = guard.as_mut() {
                self.sync_kernel_allowlist(proc).await;
            }
        }

        Ok(guard)
    }

    pub async fn reset(&self, project_root: &Path) -> Result<()> {
        let allowlist = self.allowlist.lock().await.clone();
        let mut guard = self.kernel.lock().await;
        if let Some(mut old_proc) = guard.take() {
            old_proc.kill().await;
        }
        let new_proc = RlmKernelProcess::spawn(project_root, &allowlist).await?;
        *guard = Some(new_proc);
        Ok(())
    }
}

static RLM_MANAGER: OnceLock<RlmKernelManager> = OnceLock::new();

pub fn get_rlm_manager() -> &'static RlmKernelManager {
    RLM_MANAGER.get_or_init(RlmKernelManager::new)
}

pub struct RlmPythonTool;

#[async_trait]
impl Tool for RlmPythonTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "rlm_python".to_string(),
            description: "Executes Python code inside a persistent READ-ONLY Python kernel session (variables, imports, and functions persist across calls). Reads outside the project require an approved external allowlist (use request_external_access). Writes, deletions, subprocesses, sockets, ctypes and other escape hatches are blocked. Helpers: `_rlm_load(path)` (memoized full read), `_rlm_symbols(path)` (compact path:line:definitions), `_rlm_grep(pattern, dir='.')` (capped path:line:match results), `_rlm_snippet(path, start, end)` (capped line-range read). Only stdout (print) is returned.".to_string(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "Python snippet to execute in the persistent read-only kernel"
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "description": "Execution timeout in seconds (default: 30)"
                    },
                    "reset": {
                        "type": "boolean",
                        "description": "Set to true to restart the kernel and clear memory"
                    }
                },
                "required": ["code"]
            }),
            requires_approval: false,
        }
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let code = params.get("code").and_then(|v| v.as_str()).unwrap_or("");
        let timeout_secs = params
            .get("timeout_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(30);
        let reset = params.get("reset").and_then(|v| v.as_bool()).unwrap_or(false);

        let mgr = get_rlm_manager();

        if reset {
            if let Err(e) = mgr.reset(&ctx.project_root).await {
                return Ok(ToolResult {
                    success: false,
                    output: format!("Failed to reset Python kernel: {}", e),
                    is_error: true,
                });
            }
            if code.trim().is_empty() {
                return Ok(ToolResult {
                    success: true,
                    output: "Python kernel successfully reset.".to_string(),
                    is_error: false,
                });
            }
        }

        if code.trim().is_empty() {
            return Ok(ToolResult {
                success: false,
                output: "Code parameter cannot be empty.".to_string(),
                is_error: true,
            });
        }

        let mut guard = match mgr.get_or_spawn(&ctx.project_root).await {
            Ok(g) => g,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: format!("Python Kernel Error: {}", e),
                    is_error: true,
                });
            }
        };

        let proc = match guard.as_mut() {
            Some(p) => p,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: "Python kernel process unavailable.".to_string(),
                    is_error: true,
                });
            }
        };

        // Cancellable execution: `agent_cancel_run` must interrupt a long
        // Python snippet instead of waiting out its timeout. On cancel the
        // process is killed and the kernel is reset for the next call.
        let exec_result = tokio::select! {
            biased;
            _ = ctx.cancel.notified() => None,
            res = proc.execute_user_code(code, timeout_secs) => Some(res),
        };
        let exec_result = match exec_result {
            Some(res) => res,
            None => {
                proc.kill().await;
                *guard = None;
                return Ok(ToolResult {
                    success: false,
                    output: "Run cancelled by user.".to_string(),
                    is_error: true,
                });
            }
        };

        match exec_result {
            Ok(output) => {
                let trimmed = output.trim();
                // Only tracebacks in the STDERR segment count as an error; a user
                // printing the literal string to stdout is not a failure.
                let stderr_part = trimmed.split("=== STDERR ===").nth(1).unwrap_or("");
                let has_traceback = stderr_part.contains("Traceback (most recent call last):");
                let final_output = if trimmed.is_empty() {
                    "(Code executed successfully with no stdout output. Use print(...) to inspect values)".to_string()
                } else {
                    trimmed.to_string()
                };
                Ok(ToolResult {
                    success: !has_traceback,
                    output: final_output,
                    is_error: has_traceback,
                })
            }
            Err(e) => {
                proc.kill().await;
                *guard = None;

                Ok(ToolResult {
                    success: false,
                    output: format!("Execution Error: {}. (Kernel has been reset)", e),
                    is_error: true,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_file_code(path: &Path) -> String {
        let b64 = BASE64.encode(path.to_string_lossy().as_bytes());
        format!("import base64 as _b; print(open(_b.b64decode(b'{}').decode()).read())", b64)
    }

    fn memo_load_code(path: &Path) -> String {
        let b64 = BASE64.encode(path.to_string_lossy().as_bytes());
        format!("import base64 as _b; print(_rlm_load(_b.b64decode(b'{}').decode()))", b64)
    }

    #[tokio::test]
    async fn test_add_allowed_root_rejects_broad_roots() {
        let mgr = RlmKernelManager::new();

        // Filesystem root, home directory, and system trees must never enter
        // the allowlist — even after an approval click.
        for broad in ["/", "/etc", "/usr", "/var", "/private", "/tmp"] {
            assert!(
                mgr.add_allowed_root(Path::new(broad)).await.is_err(),
                "{} must be refused",
                broad
            );
        }
        if let Some(home) = std::env::var_os("HOME") {
            assert!(
                mgr.add_allowed_root(Path::new(&home)).await.is_err(),
                "the home directory itself must be refused"
            );
        }

        // Narrow paths stay approvable.
        let narrow = std::env::temp_dir().join("kuda_rlm_narrow_allow_dir");
        let _ = std::fs::create_dir_all(&narrow);
        assert!(mgr.add_allowed_root(&narrow).await.is_ok());
        let _ = std::fs::remove_dir_all(&narrow);
    }

    #[tokio::test]
    async fn test_rlm_python_execution() {
        let temp_dir = std::env::temp_dir().join("rlm_kernel_test");
        let _ = std::fs::create_dir_all(&temp_dir);

        let mgr = RlmKernelManager::new();
        {
            let mut guard = mgr.get_or_spawn(&temp_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();

            let out1 = proc.execute_code("x = 42\nprint(f'VAL:{x}')", 10).await.unwrap();
            assert!(out1.contains("VAL:42"));

            let out2 = proc.execute_code("print(f'PERSIST:{x + 10}')", 10).await.unwrap();
            assert!(out2.contains("PERSIST:52"));

            proc.kill().await;
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_rlm_python_error_reporting_and_recovery() {
        let temp_dir = std::env::temp_dir().join("rlm_kernel_test_err");
        let _ = std::fs::create_dir_all(&temp_dir);

        let mgr = RlmKernelManager::new();
        {
            let mut guard = mgr.get_or_spawn(&temp_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();

            let out = proc.execute_code("raise ValueError('rlm_boom')", 10).await.unwrap();
            assert!(out.contains("ValueError: rlm_boom"), "stderr traceback must surface: {}", out);

            let out2 = proc.execute_code("print('still_alive')", 10).await.unwrap();
            assert!(out2.contains("still_alive"), "kernel must survive an exception");

            proc.kill().await;
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_rlm_python_readonly_guard() {
        let temp_dir = std::env::temp_dir().join("rlm_kernel_test_readonly");
        let _ = std::fs::create_dir_all(&temp_dir);

        let mgr = RlmKernelManager::new();
        {
            let mut guard = mgr.get_or_spawn(&temp_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();

            let out_open = proc.execute_code("open('test.txt', 'w')", 10).await.unwrap();
            assert!(out_open.contains("ReadOnlyError"), "open('w') must be blocked by ReadOnlyError: {}", out_open);

            let out_remove = proc.execute_code("import os\nos.remove('test.txt')", 10).await.unwrap();
            assert!(out_remove.contains("ReadOnlyError"), "os.remove must be blocked by ReadOnlyError: {}", out_remove);

            proc.kill().await;
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_rlm_guard_originals_not_accessible() {
        let temp_dir = std::env::temp_dir().join("rlm_kernel_test_bypass");
        let _ = std::fs::create_dir_all(&temp_dir);

        let mgr = RlmKernelManager::new();
        {
            let mut guard = mgr.get_or_spawn(&temp_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();

            // The saved originals (`_orig_open` etc.) must NOT leak into the
            // session globals — they used to, letting model code read any file.
            let out = proc
                .execute_code("print('HAS_ORIG', '_orig_open' in globals())", 10)
                .await
                .unwrap();
            assert!(
                out.contains("HAS_ORIG False"),
                "guard originals leaked into globals: {}",
                out
            );

            // `importlib.reload(os)` must not be able to restore the unpatched
            // os attributes.
            let out2 = proc
                .execute_code(
                    "import importlib, os\ntry:\n    importlib.reload(os)\n    print('RELOAD_OK')\nexcept Exception:\n    print('RELOAD_BLOCKED')",
                    10,
                )
                .await
                .unwrap();
            assert!(
                out2.contains("RELOAD_BLOCKED"),
                "importlib.reload must be blocked: {}",
                out2
            );

            // `sys` must be blocked: it is the classic route to the FULL session
            // globals via `sys.modules['__main__'].__dict__` (which would allow
            // the model to reassign `_rlm_allowlist` / `_rlm_project_root`).
            let out3 = proc
                .execute_code(
                    "import sys\nprint('SYS_IMPORT_OK')",
                    10,
                )
                .await
                .unwrap();
            assert!(
                out3.contains("ImportError") || out3.contains("Error"),
                "sys import must be blocked: {}",
                out3
            );

            // `import posix` must be blocked: the guard patches `os.*`, but the
            // C module `posix` backs those functions — an unpatched `posix`
            // would read/delete ANY file (`posix.open('/etc/passwd', 0)`).
            let out4 = proc
                .execute_code(
                    "try:\n    import posix\n    print('POSIX_OK')\nexcept ImportError:\n    print('POSIX_BLOCKED')",
                    10,
                )
                .await
                .unwrap();
            assert!(
                out4.contains("POSIX_BLOCKED"),
                "posix import must be blocked: {}",
                out4
            );

            // `gc` / `inspect` escape hatches must be blocked too.
            let out5 = proc
                .execute_code(
                    "import gc as _g, inspect as _i\nprint('GC_INSPECT_OK')",
                    10,
                )
                .await
                .unwrap();
            assert!(
                out5.contains("ImportError") || out5.contains("Error"),
                "gc/inspect imports must be blocked: {}",
                out5
            );

            proc.kill().await;
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_rlm_user_code_cannot_mutate_allowlist() {
        let project_dir = std::env::temp_dir().join("rlm_kernel_test_userns");
        let external_dir = std::env::temp_dir().join("rlm_kernel_test_userns_ext");
        let _ = std::fs::create_dir_all(&project_dir);
        let _ = std::fs::create_dir_all(&external_dir);
        let external_file = external_dir.join("secret.txt");
        let _ = std::fs::write(&external_file, "topsecret");

        let mgr = RlmKernelManager::new();
        {
            let mut guard = mgr.get_or_spawn(&project_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();

            // Model code runs in a restricted namespace: the sandbox's own
            // state (`_rlm_allowlist`, `_rlm_project_root`) must not be visible.
            let out = proc
                .execute_user_code(
                    "print('HAS_ALLOW', '_rlm_allowlist' in globals())\nprint('HAS_ROOT', '_rlm_project_root' in globals())",
                    10,
                )
                .await
                .unwrap();
            assert!(
                out.contains("HAS_ALLOW False"),
                "allowlist must be hidden from user code: {}",
                out
            );
            assert!(
                out.contains("HAS_ROOT False"),
                "project root must be hidden from user code: {}",
                out
            );

            // Even if the model tries to fabricate a broad allowlist entry inside
            // its (throwaway) namespace, the real guard state is untouched: the
            // external file must STILL be unreadable.
            let out2 = proc
                .execute_user_code(
                    "_rlm_allowlist = ['/']\n_rlm_project_root = '/'\n",
                    10,
                )
                .await
                .unwrap();
            let out3 = proc
                .execute_user_code(&memo_load_code(&external_file), 10)
                .await
                .unwrap();
            assert!(
                out3.contains("READ BLOCKED_EXTERNAL") || out3.contains("ReadOnlyError"),
                "allowlist mutation attempt must not widen the read scope: {}",
                out3
            );
            let _ = out2;

            // `import sys` must be blocked so the full session globals cannot be
            // reached via sys.modules['__main__'].
            let out4 = proc
                .execute_user_code("import sys\nprint('SYS_OK')", 10)
                .await
                .unwrap();
            assert!(
                out4.contains("ImportError") || out4.contains("Error"),
                "sys import must be blocked from user code: {}",
                out4
            );

            proc.kill().await;
        }

        let _ = std::fs::remove_dir_all(&project_dir);
        let _ = std::fs::remove_dir_all(&external_dir);
    }

    #[tokio::test]
    async fn test_rlm_user_code_globals_attack_blocked() {
        // Regression for the one-step sandbox escape: `_rlm_load.__globals__`
        // used to BE the session namespace, so `['_rlm_allowlist'] = ['']`
        // widened the read scope ('' + os.sep == '/', matching every absolute
        // path). Helpers are now rebuilt against a private globals dict that has
        // no `_rlm_allowlist` / `_rlm_project_root` keys, so the attack must
        // fail with KeyError and the external file must stay unreadable.
        let project_dir = std::env::temp_dir().join("rlm_kernel_test_globals_attack");
        let external_dir = std::env::temp_dir().join("rlm_kernel_test_globals_attack_ext");
        let _ = std::fs::create_dir_all(&project_dir);
        let _ = std::fs::create_dir_all(&external_dir);
        let external_file = external_dir.join("secret.txt");
        let _ = std::fs::write(&external_file, "topsecret");

        let mgr = RlmKernelManager::new();
        {
            let mut guard = mgr.get_or_spawn(&project_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();

            // The session allowlist must not be reachable through a helper's
            // `__globals__`. (`_rlm_project_root` is deliberately present as a
            // plain value — `_rlm_rel` needs it for path display — but the
            // guard checks read the root from their closure, so rebinding the
            // private copy cannot widen the scope.)
            let out = proc
                .execute_user_code(
                    "print('HAS_ALLOW', '_rlm_allowlist' in _rlm_load.__globals__)\nprint('HAS_BUILTINS', 'builtins' in _rlm_load.__globals__)",
                    10,
                )
                .await
                .unwrap();
            assert!(
                out.contains("HAS_ALLOW False"),
                "allowlist must be hidden from helper __globals__: {}",
                out
            );
            assert!(
                out.contains("HAS_BUILTINS False"),
                "the session builtins module must be hidden from helper __globals__: {}",
                out
            );

            // The exact former escape: rebind `_rlm_allowlist` to `['']` (which
            // used to make every absolute path "in scope"), then read the
            // external file. Also try rebinding `_rlm_project_root` in the
            // private globals to `''`. Neither rebind may widen anything: the
            // private globals dict is not what the guard checks consult (they
            // read the allowlist/root from their closure), so the read must
            // still be blocked.
            let attack = format!(
                "import base64 as _b\n\
                 try:\n    _rlm_load.__globals__['_rlm_allowlist'] = ['']\n    print('REBIND_DONE')\n\
                 except Exception:\n    print('REBIND_FAILED')\n\
                 _rlm_load.__globals__['_rlm_project_root'] = ''\n\
                 print(_rlm_load(_b.b64decode(b'{}').decode()))",
                BASE64.encode(external_file.to_string_lossy().as_bytes())
            );
            let out2 = proc.execute_user_code(&attack, 10).await.unwrap();
            assert!(
                out2.contains("READ BLOCKED_EXTERNAL") || out2.contains("ReadOnlyError"),
                "external read must still be blocked after the globals-rebind attack: {}",
                out2
            );

            // The next snippet must be equally unaffected (no residual widening).
            let out3 = proc
                .execute_user_code(&memo_load_code(&external_file), 10)
                .await
                .unwrap();
            assert!(
                out3.contains("READ BLOCKED_EXTERNAL") || out3.contains("ReadOnlyError"),
                "scope must stay intact across snippets: {}",
                out3
            );

            proc.kill().await;
        }

        let _ = std::fs::remove_dir_all(&project_dir);
        let _ = std::fs::remove_dir_all(&external_dir);
    }

    #[tokio::test]
    async fn test_rlm_env_sanitized() {
        let temp_dir = std::env::temp_dir().join("rlm_kernel_test_env");
        let _ = std::fs::create_dir_all(&temp_dir);
        // Simulate a secret inherited from the IDE process.
        unsafe {
            std::env::set_var("KUDA_RLM_TEST_SECRET_KEY", "sk-super-secret");
            std::env::set_var("KUDA_RLM_TEST_DB_URL", "postgres://u:p@db");
        }

        let mgr = RlmKernelManager::new();
        {
            let mut guard = mgr.get_or_spawn(&temp_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();
            let out = proc
                .execute_code(
                    "import os\nprint('SECRET=', os.environ.get('KUDA_RLM_TEST_SECRET_KEY'))\nprint('DB=', os.environ.get('KUDA_RLM_TEST_DB_URL'))\nprint('PATH_OK=', 'PATH' in os.environ)",
                    10,
                )
                .await
                .unwrap();
            assert!(
                out.contains("SECRET= None") && out.contains("DB= None"),
                "inherited secrets must be stripped from kernel env: {}",
                out
            );
            assert!(out.contains("PATH_OK= True"), "safe vars must survive: {}", out);
            proc.kill().await;
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_rlm_kill_and_abort_blocked() {
        let temp_dir = std::env::temp_dir().join("rlm_kernel_test_kill");
        let _ = std::fs::create_dir_all(&temp_dir);

        let mgr = RlmKernelManager::new();
        {
            let mut guard = mgr.get_or_spawn(&temp_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();

            let out_kill = proc.execute_code("import os\nos.kill(1, 9)", 10).await.unwrap();
            assert!(
                out_kill.contains("ReadOnlyError"),
                "os.kill must be blocked: {}",
                out_kill
            );
            let out_abort = proc.execute_code("import os\nos.abort()", 10).await.unwrap();
            assert!(
                out_abort.contains("ReadOnlyError"),
                "os.abort must be blocked: {}",
                out_abort
            );
            let out_exit = proc.execute_code("import os\nos._exit(0)", 10).await.unwrap();
            assert!(
                out_exit.contains("ReadOnlyError"),
                "os._exit must be blocked: {}",
                out_exit
            );
            // The kernel must still be alive after all three attempts.
            let out_alive = proc.execute_code("print('ALIVE')", 10).await.unwrap();
            assert!(out_alive.contains("ALIVE"), "kernel must survive: {}", out_alive);

            proc.kill().await;
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_rlm_metadata_probes_blocked() {
        let temp_dir = std::env::temp_dir().join("rlm_kernel_test_stat");
        let _ = std::fs::create_dir_all(&temp_dir);
        let inside = temp_dir.join("inside.txt");
        let _ = std::fs::write(&inside, "hello");

        let mgr = RlmKernelManager::new();
        {
            let mut guard = mgr.get_or_spawn(&temp_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();

            // stat/exists on sensitive paths must be refused even without a read.
            for snippet in [
                "import os\nos.stat('/etc/hosts')",
                "import os\nos.path.exists('/etc/hosts')",
                "import os\nos.path.getsize('/etc/hosts')",
                "import os\nos.path.isfile('/etc/hosts')",
            ] {
                let out = proc.execute_code(snippet, 10).await.unwrap();
                assert!(
                    out.contains("ReadOnlyError"),
                    "metadata probe must be blocked: {} -> {}",
                    snippet,
                    out
                );
            }

            // In-project metadata stays available.
            let out_ok = proc
                .execute_code(
                    &format!("import os\nprint('SIZE', os.path.getsize({:?}))", inside.to_string_lossy()),
                    10,
                )
                .await
                .unwrap();
            assert!(out_ok.contains("SIZE 5"), "in-scope stat must work: {}", out_ok);

            proc.kill().await;
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_rlm_allowlist_is_immutable_and_builtins_import_blocked() {
        let temp_dir = std::env::temp_dir().join("rlm_kernel_test_imm");
        let _ = std::fs::create_dir_all(&temp_dir);

        let mgr = RlmKernelManager::new();
        {
            let mut guard = mgr.get_or_spawn(&temp_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();

            // The allowlist container has no mutation API, so even a leaked
            // reference cannot widen the read scope.
            let out = proc
                .execute_code(
                    "try:\n    _rlm_allowlist.append('/')\n    print('APPEND_OK')\nexcept Exception:\n    print('APPEND_BLOCKED')",
                    10,
                )
                .await
                .unwrap();
            assert!(
                out.contains("APPEND_BLOCKED"),
                "allowlist mutation must be refused: {}",
                out
            );

            // `import builtins` is refused outright.
            let out2 = proc
                .execute_code(
                    "try:\n    import builtins\n    print('BUILTINS_OK')\nexcept ImportError:\n    print('BUILTINS_BLOCKED')",
                    10,
                )
                .await
                .unwrap();
            assert!(
                out2.contains("BUILTINS_BLOCKED"),
                "import builtins must be refused: {}",
                out2
            );

            proc.kill().await;
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_rlm_user_code_has_no_open_primitive() {
        let temp_dir = std::env::temp_dir().join("rlm_kernel_test_noopen");
        let _ = std::fs::create_dir_all(&temp_dir);

        let mgr = RlmKernelManager::new();
        {
            let mut guard = mgr.get_or_spawn(&temp_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();

            // The raw `open` builtin is stripped from user-code namespaces; the
            // model reads through `_rlm_load` instead.
            let out = proc
                .execute_user_code("print(open('/etc/hosts').read())", 10)
                .await
                .unwrap();
            assert!(
                out.contains("NameError") || out.contains("not defined"),
                "raw open must be unavailable in user code: {}",
                out
            );

            // `_rlm_load` still works inside the restricted namespace.
            let inside = temp_dir.join("f.txt");
            let _ = std::fs::write(&inside, "data");
            let out2 = proc
                .execute_user_code(&memo_load_code(&inside), 10)
                .await
                .unwrap();
            assert!(out2.contains("data"), "_rlm_load must still work: {}", out2);

            proc.kill().await;
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }


    #[tokio::test]
    async fn test_rlm_python_external_read_blocked_then_allowed() {
        let project_dir = std::env::temp_dir().join("rlm_kernel_test_scope");
        let external_dir = std::env::temp_dir().join("rlm_kernel_test_external");
        let _ = std::fs::create_dir_all(&project_dir);
        let _ = std::fs::create_dir_all(&external_dir);
        let external_file = external_dir.join("data.txt");
        let _ = std::fs::write(&external_file, "external_content");

        let mgr = RlmKernelManager::new();
        {
            let mut guard = mgr.get_or_spawn(&project_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();
            let out = proc.execute_code(&read_file_code(&external_file), 10).await.unwrap();
            assert!(
                out.contains("READ BLOCKED_EXTERNAL") || out.contains("ReadOnlyError"),
                "out-of-scope read must be blocked: {}",
                out
            );
            proc.kill().await;
        }

        // Allow the external dir; the next spawn injects it into the guard.
        mgr.add_allowed_root(&external_dir).await.unwrap();
        {
            let mut guard = mgr.get_or_spawn(&project_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();
            let out = proc.execute_code(&read_file_code(&external_file), 10).await.unwrap();
            assert!(out.contains("external_content"), "after allow, read must succeed: {}", out);
            proc.kill().await;
        }

        // Clear the allowlist; live-clear the running kernel.
        mgr.reset_allowlist().await.unwrap();
        {
            let mut guard = mgr.get_or_spawn(&project_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();
            let out = proc.execute_code(&read_file_code(&external_file), 10).await.unwrap();
            assert!(
                out.contains("READ BLOCKED_EXTERNAL") || out.contains("ReadOnlyError"),
                "after reset, read must be blocked again: {}",
                out
            );
            proc.kill().await;
        }

        let _ = std::fs::remove_dir_all(&project_dir);
        let _ = std::fs::remove_dir_all(&external_dir);
    }

    #[tokio::test]
    async fn test_rlm_python_allow_non_existent_path_takes_effect() {
        let project_dir = std::env::temp_dir().join("rlm_kernel_test_missing_allow");
        let external_dir = std::env::temp_dir().join("rlm_kernel_test_missing_allow_ext");
        let _ = std::fs::create_dir_all(&project_dir);
        let _ = std::fs::create_dir_all(&external_dir);
        let missing = external_dir.join("not_generated.yaml");

        let mgr = RlmKernelManager::new();
        {
            let mut guard = mgr.get_or_spawn(&project_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();
            let out = proc.execute_code(&read_file_code(&missing), 10).await.unwrap();
            assert!(
                out.contains("READ BLOCKED_EXTERNAL") || out.contains("ReadOnlyError"),
                "out-of-scope read must be blocked before approval: {}",
                out
            );
            proc.kill().await;
        }

        // Approving a path that does not exist on disk must still take effect:
        // the kernel should consider it in scope and surface the real error
        // (FileNotFoundError) instead of a misleading "outside scope" block.
        mgr.add_allowed_root(&missing).await.unwrap();
        {
            let mut guard = mgr.get_or_spawn(&project_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();
            let out = proc.execute_code(&read_file_code(&missing), 10).await.unwrap();
            assert!(
                !out.contains("BLOCKED_EXTERNAL"),
                "approved non-existent path must not be blocked as out of scope: {}",
                out
            );
            assert!(
                out.contains("FileNotFoundError") || out.contains("No such file"),
                "kernel should report the missing file, not a scope violation: {}",
                out
            );
            proc.kill().await;
        }

        let _ = std::fs::remove_dir_all(&project_dir);
        let _ = std::fs::remove_dir_all(&external_dir);
    }

    #[tokio::test]
    async fn test_rlm_python_approval_survives_kernel_respawn() {
        let project_dir = std::env::temp_dir().join("rlm_kernel_test_approval_respawn");
        let external_dir = std::env::temp_dir().join("rlm_kernel_test_approval_respawn_ext");
        let _ = std::fs::create_dir_all(&project_dir);
        let _ = std::fs::create_dir_all(&external_dir);
        let external_file = external_dir.join("data.txt");
        let _ = std::fs::write(&external_file, "respawn_content");

        let mgr = RlmKernelManager::new();
        mgr.add_allowed_root(&external_file).await.unwrap();

        // Kill the kernel in between approvals/reads; the next get_or_spawn must
        // respawn WITH the approved paths injected (no re-approval needed).
        {
            let mut guard = mgr.get_or_spawn(&project_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();
            let out = proc.execute_code(&read_file_code(&external_file), 10).await.unwrap();
            assert!(out.contains("respawn_content"), "approved read must work: {}", out);
            proc.kill().await;
        }
        {
            let mut guard = mgr.get_or_spawn(&project_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();
            let out = proc.execute_code(&read_file_code(&external_file), 10).await.unwrap();
            assert!(out.contains("respawn_content"), "approval must survive respawn: {}", out);
            proc.kill().await;
        }

        let _ = std::fs::remove_dir_all(&project_dir);
        let _ = std::fs::remove_dir_all(&external_dir);
    }

    #[tokio::test]
    async fn test_rlm_python_memo_load_reloads_on_change() {
        let temp_dir = std::env::temp_dir().join("rlm_kernel_test_memo");
        let _ = std::fs::create_dir_all(&temp_dir);
        let target = temp_dir.join("memo.txt");
        let _ = std::fs::write(&target, "v1\n");

        let mgr = RlmKernelManager::new();
        {
            let mut guard = mgr.get_or_spawn(&temp_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();

            let out1 = proc.execute_code(&memo_load_code(&target), 10).await.unwrap();
            assert!(out1.contains("v1"), "first load must return v1: {}", out1);

            // Modify the file -> memo must invalidate and reload.
            let _ = std::fs::write(&target, "v2\n");
            let out2 = proc.execute_code(&memo_load_code(&target), 10).await.unwrap();
            assert!(out2.contains("v2"), "memo should reload after change: {}", out2);

            proc.kill().await;
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_rlm_python_memo_detects_change_with_preserved_mtime() {
        let temp_dir = std::env::temp_dir().join("rlm_kernel_test_memo_mtime");
        let _ = std::fs::create_dir_all(&temp_dir);
        let target = temp_dir.join("memo.txt");
        let _ = std::fs::write(&target, "v1\n");

        let mgr = RlmKernelManager::new();
        {
            let mut guard = mgr.get_or_spawn(&temp_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();

            let out1 = proc.execute_code(&memo_load_code(&target), 10).await.unwrap();
            assert!(out1.contains("v1"), "first load must return v1: {}", out1);

            // Rewrite with different content, then restore the file mtime to a
            // fixed past value (simulates `cp -p` / `rsync --times`). The memo
            // must NOT serve the stale v1 content.
            let _ = std::fs::write(&target, "a longer second version\n");
            let touch = std::process::Command::new("touch")
                .args(["-t", "202001010000"])
                .arg(&target)
                .status();
            if let Ok(status) = touch {
                assert!(status.success());
            }

            let out2 = proc.execute_code(&memo_load_code(&target), 10).await.unwrap();
            assert!(
                out2.contains("a longer second version"),
                "memo must reload when content changed despite preserved mtime: {}",
                out2
            );

            proc.kill().await;
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_rlm_python_prewarm_loads_files() {
        let temp_dir = std::env::temp_dir().join("rlm_kernel_test_prewarm");
        let _ = std::fs::create_dir_all(&temp_dir);
        let f1 = temp_dir.join("a.txt");
        let _ = std::fs::write(&f1, "alpha");
        let f2 = temp_dir.join("b.txt");
        let _ = std::fs::write(&f2, "beta");
        let missing = temp_dir.join("missing.txt");

        let mgr = RlmKernelManager::new();
        let paths = vec![
            f1.to_string_lossy().to_string(),
            f2.to_string_lossy().to_string(),
            missing.to_string_lossy().to_string(),
        ];
        let loaded = mgr.prewarm(&temp_dir, &paths).await.unwrap();
        assert_eq!(loaded, 2, "existing files must be loaded, missing skipped");

        let mut guard = mgr.get_or_spawn(&temp_dir).await.unwrap();
        let proc = guard.as_mut().unwrap();
        let out = proc.execute_code("print(len(_rlm_index))", 10).await.unwrap();
        assert!(out.contains("2"), "kernel should hold 2 cached files: {}", out);
        proc.kill().await;

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_rlm_python_helpers_symbols_grep_snippet() {
        let temp_dir = std::env::temp_dir().join("rlm_kernel_test_helpers");
        let _ = std::fs::create_dir_all(&temp_dir);
        let target = temp_dir.join("sample.py");
        let _ = std::fs::write(
            &target,
            "import os\n\n\ndef alpha(x):\n    return x + 1\n\n\nclass Beta:\n    def method(self):\n        pass\n",
        );

        let mgr = RlmKernelManager::new();
        {
            let mut guard = mgr.get_or_spawn(&temp_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();

            let out_symbols = proc
                .execute_code("print(_rlm_symbols('sample.py'))", 10)
                .await
                .unwrap();
            assert!(out_symbols.contains("sample.py:4:def alpha"), "symbols must find def alpha: {}", out_symbols);
            assert!(out_symbols.contains("class Beta"), "symbols must find class Beta: {}", out_symbols);

            let out_grep = proc
                .execute_code("print(_rlm_grep('alpha', '.'))", 10)
                .await
                .unwrap();
            assert!(out_grep.contains("sample.py:4"), "grep must locate the symbol: {}", out_grep);

            let out_snippet = proc
                .execute_code("print(_rlm_snippet('sample.py', 4, 5))", 10)
                .await
                .unwrap();
            assert!(out_snippet.contains("def alpha"), "snippet must return the requested lines: {}", out_snippet);
            assert!(out_snippet.contains("[4-5]"), "snippet must echo its range: {}", out_snippet);

            proc.kill().await;
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_rlm_python_helpers_cap_output() {
        let temp_dir = std::env::temp_dir().join("rlm_kernel_test_helpers_cap");
        let _ = std::fs::create_dir_all(&temp_dir);
        let target = temp_dir.join("wide.py");
        let mut content = String::new();
        // Big enough to exceed the (raised) 24k helper cap: ~3000 def lines.
        for i in 0..3000 {
            content.push_str(&format!("def fn_{}(x):\n    return x + {}\n\n", i, i));
        }
        let _ = std::fs::write(&target, &content);

        let mgr = RlmKernelManager::new();
        {
            let mut guard = mgr.get_or_spawn(&temp_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();
            let out = proc
                .execute_code("print(_rlm_symbols('wide.py'))", 10)
                .await
                .unwrap();
            assert!(
                out.contains("TRUNCATED"),
                "helper output must be capped: {}",
                out
            );
            proc.kill().await;
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_rlm_python_helpers_respect_scope() {
        let project_dir = std::env::temp_dir().join("rlm_kernel_test_helpers_scope");
        let external_dir = std::env::temp_dir().join("rlm_kernel_test_helpers_external");
        let _ = std::fs::create_dir_all(&project_dir);
        let _ = std::fs::create_dir_all(&external_dir);
        let ext = external_dir.join("secret.py");
        let _ = std::fs::write(&ext, "def hidden():\n    pass\n");

        let mgr = RlmKernelManager::new();
        {
            let mut guard = mgr.get_or_spawn(&project_dir).await.unwrap();
            let proc = guard.as_mut().unwrap();

            let out_syms = proc
                .execute_code(&format!(
                    "import base64 as _b\nprint(_rlm_symbols(_b.b64decode(b'{}').decode()))",
                    BASE64.encode(ext.to_string_lossy().as_bytes())
                ), 10)
                .await
                .unwrap();
            assert!(
                out_syms.contains("BLOCKED_EXTERNAL") || out_syms.contains("ReadOnlyError"),
                "out-of-scope _rlm_symbols must be blocked: {}",
                out_syms
            );

            let out_grep = proc
                .execute_code("print(_rlm_grep('def hidden', '.'))", 10)
                .await
                .unwrap();
            assert!(
                !out_grep.contains("secret.py"),
                "grep must not leak files outside the project: {}",
                out_grep
            );
            proc.kill().await;
        }

        let _ = std::fs::remove_dir_all(&project_dir);
        let _ = std::fs::remove_dir_all(&external_dir);
    }
}
