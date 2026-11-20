"""Bound command output and lifetime; never invoke a shell.

Git, GitHub, service control and isolated source checks share this boundary.
Callers retaining a transcript own its private file and durability.
Failed reads kill their owned process group, including inherited-pipe children.
"""
import contextlib
import errno
import os
import selectors
import select
import signal
import subprocess
import sys
import tempfile
import time

CLEANUP_SECONDS = 5
MAX_INPUT = 8192


def exited(process):
    """Observe termination without reaping the owned process/group identity."""
    if process.returncode is not None:
        raise RuntimeError('Owned process was reaped before group retirement')
    if sys.platform=='darwin':
        event=select.kevent(process.pid,filter=select.KQ_FILTER_PROC,
                           flags=select.KQ_EV_ADD|select.KQ_EV_ONESHOT,fflags=select.KQ_NOTE_EXIT)
        with contextlib.closing(select.kqueue()) as queue:
            try:
                events=queue.control([event],1,0)
            except ProcessLookupError:
                # An unreaped child cannot have its PID reused. Attaching to
                # its already-exited process may report ESRCH instead of EXIT.
                return True
        if not events:
            return False
        observed=events[0]
        if observed.flags&select.KQ_EV_ERROR:
            if observed.data==errno.ESRCH:
                return True
            raise OSError(observed.data,'Owned child exit observation failed')
        if observed.ident!=process.pid or not observed.fflags&select.KQ_NOTE_EXIT:
            raise RuntimeError('Unexpected owned child lifetime event')
        return True
    if sys.platform!='linux':
        raise ValueError('Owned process observation requires macOS or Linux')
    return os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOWAIT | os.WNOHANG) is not None


@contextlib.contextmanager
def control_input(data):
    if data is None:
        yield subprocess.DEVNULL
        return
    if not isinstance(data, bytes) or len(data) > MAX_INPUT:
        raise ValueError('Control command input exceeds its byte limit')
    with tempfile.TemporaryFile() as stream:
        stream.write(data)
        stream.seek(0)
        yield stream


def departed_group(pid):
    """macOS can return EPERM for an unreaped group's sole zombie leader."""
    if sys.platform != 'darwin':
        return False
    import ctypes
    library = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True)
    query = library.proc_listpgrppids
    query.argtypes = [ctypes.c_int32, ctypes.c_void_p, ctypes.c_int]
    query.restype = ctypes.c_int
    # Apple's wrapper returns a PID count, not a byte count.
    # We only accept an empty group or the one departed leader. Any additional
    # member, including a truncated list, keeps cleanup explicitly uncertain.
    members = (ctypes.c_int32 * 2)()
    ctypes.set_errno(0)
    count = query(pid, members, ctypes.sizeof(members))
    if ctypes.get_errno() or count not in (0, 1):
        return False
    if count == 0:
        return True
    if members[0] != pid:
        return False
    try:
        os.getpgid(pid)
    except ProcessLookupError:
        return True
    return False


def retire(process):
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    except PermissionError:
        if not departed_group(process.pid):
            raise
    process.wait(timeout=CLEANUP_SECONDS)


def capture(command, *, seconds, limit, input_bytes=None, cwd=None, merge_stderr=False, transcript=None, started=None):
    if not 0 < seconds <= 90*60 or type(limit) is not int or not 0 < limit <= 16*1024*1024:
        raise ValueError('Invalid control-command deadline or output limit')
    until, output = time.monotonic()+seconds, bytearray()
    with control_input(input_bytes) as request:
        process = subprocess.Popen(command, stdin=request, stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT if merge_stderr else subprocess.DEVNULL,
                                   start_new_session=True, cwd=cwd)
    try:
        if started is not None:
            started(process.pid)
        with selectors.DefaultSelector() as ready:
            ready.register(process.stdout, selectors.EVENT_READ)
            while ready.get_map():
                remaining = until-time.monotonic()
                if remaining <= 0 or not ready.select(remaining):
                    raise TimeoutError('Control command exceeded its deadline')
                chunk = os.read(process.stdout.fileno(), 65536)
                if not chunk:
                    ready.unregister(process.stdout)
                available = limit-len(output)
                if transcript is not None:
                    transcript.write(chunk[:available])
                if len(chunk) > available:
                    raise ValueError('Control command response exceeds its byte limit')
                output.extend(chunk)
            result = process.wait(timeout=max(0.001, until-time.monotonic()))
    except BaseException:
        retire(process)
        raise
    finally:
        process.stdout.close()
    return result, bytes(output)
