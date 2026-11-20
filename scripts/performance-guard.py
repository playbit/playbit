#!/usr/bin/env python3
"""Run an operator cohort only while unrelated CPU activity stays calm.

Records ps's recent CPU estimates once a second. They are interference
diagnostics, not benchmark process CPU qualification. A process over one core
or an unrelated aggregate over four cores invalidates the cohort. Observed
build, test and archive jobs are rejected above half a core. SIGINT asks
the operator to stop its producers and drain exact test owners; no user process
is signalled. Exit 75 means retry with a new profile after the machine calms.
The child must handle SIGINT and retain partial evidence without replay. Calm
admission lasts at most a minute. The whole cohort has a one-hour operational
ceiling, including a thirty-second cooperative shutdown and owned-group cleanup.
Logs stop at 16 MiB; exhausting a bound invalidates the attempt.
"""
import argparse
import json
import math
import os
from pathlib import Path
import signal
import stat
import subprocess
import sys
import tempfile
import time

from process_capture import capture, exited, retire, CLEANUP_SECONDS

MAX_PROCESSES=4096
MAX_OBSERVATION_BYTES=4*1024*1024
OBSERVATION_SECONDS=5
MAX_LOG_BYTES=16*1024*1024
FINAL_LOG_BYTES=4096
CALM_SECONDS=60
MAX_SECONDS=60*60
CANCEL_SECONDS=30


class GuardLimit(RuntimeError):
    pass


class Log:
    def __init__(self,path):
        descriptor=os.open(path,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,0o600)
        self.stream=os.fdopen(descriptor,'wb')
        self.bytes=0

    def write(self,value,*,final=False):
        data=(json.dumps(value,ensure_ascii=False,allow_nan=False)+'\n').encode()
        limit=MAX_LOG_BYTES-(0 if final else FINAL_LOG_BYTES)
        if self.bytes+len(data)>limit:
            raise GuardLimit('Performance observation log limit reached')
        self.stream.write(data); self.stream.flush()
        self.bytes+=len(data)

    def close(self):
        try:
            self.stream.flush(); os.fsync(self.stream.fileno())
        finally:
            self.stream.close()


def snapshot():
    code,raw=capture(["/bin/ps", "-Ao", "pid=,ppid=,pcpu=,comm="],
                     seconds=OBSERVATION_SECONDS,limit=MAX_OBSERVATION_BYTES)
    if code:
        raise GuardLimit('Process observation failed')
    records = []
    for line in raw.decode().splitlines():
        if len(records)>=MAX_PROCESSES:
            raise GuardLimit('Process observation count limit reached')
        pid, parent, cpu, executable = line.strip().split(None, 3)
        record=dict(pid=int(pid), parent=int(parent), cpu=float(cpu), executable=executable)
        if (not 0<record['pid']<2**31 or not 0<=record['parent']<2**31 or not math.isfinite(record['cpu'])
                or record['cpu']<0 or len(executable.encode())>4096):
            raise GuardLimit('Invalid process observation')
        records.append(record)
    return records


def unrelated(records, operator, profile=None):
    if len(records)>MAX_PROCESSES:
        raise GuardLimit('Process observation count limit reached')
    owned = {os.getpid(), operator} if operator else {os.getpid()}
    for _ in range(MAX_PROCESSES):
        children = {r["pid"] for r in records if r["parent"] in owned}
        if children <= owned:
            break
        owned |= children
    # LaunchServices reparents test GUIs. Exclude only this operator's declared
    # profile; another benchmark is competing work even with the same prefix.
    return [r for r in records if r["pid"] not in owned and not (
        profile and r["executable"].startswith(str(profile) + "/apps/"))]


def heavy(records):
    for record in records:
        executable = record.get("executable", "")
        build_or_archive = Path(executable).name in (
            "rustc", "clippy-driver", "swift-frontend", "clang", "clang++", "tar")
        test_binary = "/target/" in executable and "/deps/" in executable
        if record["cpu"] > 100 or record["cpu"] > 50 and (build_or_archive or test_binary):
            return True
    return sum(r["cpu"] for r in records) > 400


def control_text(path,limit):
    try:
        descriptor=os.open(path,os.O_RDONLY|os.O_NONBLOCK|os.O_NOFOLLOW|os.O_CLOEXEC)
    except FileNotFoundError:
        return None
    with os.fdopen(descriptor,'rb') as stream:
        info=os.fstat(stream.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_uid!=os.getuid() or info.st_nlink!=1
                or info.st_mode&0o022 or not 0<=info.st_size<=limit):
            raise GuardLimit('Invalid performance control file')
        if info.st_size==0:
            return None
        value=stream.read(limit+1)
        if len(value)!=info.st_size:
            raise GuardLimit('Performance control file changed during observation')
    return value.decode()


def permit_measurement(path):
    descriptor=os.open(path,os.O_WRONLY|os.O_NONBLOCK|os.O_NOFOLLOW|os.O_CLOEXEC)
    with os.fdopen(descriptor,'wb') as stream:
        info=os.fstat(stream.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_uid!=os.getuid() or info.st_nlink!=1
                or info.st_mode&0o022 or not 0<info.st_size<=5):
            raise GuardLimit('Invalid performance measurement gate')
        stream.write(b'go'); stream.truncate()


def finish_operator(child,cancel_until):
    previous={number:signal.signal(number,signal.SIG_IGN) for number in (signal.SIGINT,signal.SIGTERM)}
    try:
        try:
            if not exited(child):
                if cancel_until is None:
                    cancel_until=time.monotonic()+CANCEL_SECONDS
                    child.send_signal(signal.SIGINT)
                while not exited(child) and time.monotonic()<cancel_until:
                    time.sleep(min(0.1,max(0,cancel_until-time.monotonic())))
        finally:
            # The unreaped leader pins the group even if observation itself
            # failed. Never signal a group after anyone reaped that identity.
            if child.returncode is None:
                retire(child)
    finally:
        for number,handler in previous.items():
            signal.signal(number,handler)


def run(args,command,log):
    started=time.monotonic()
    # Reserve cooperative shutdown, one final observation and both the failed
    # observer's and operator's cleanup. No deadline is restarted after failure.
    until=started+MAX_SECONDS-CANCEL_SECONDS-2*CLEANUP_SECONDS-OBSERVATION_SECONDS
    child=None
    directory=None
    cancel_until=None
    contaminated=False
    failure=None
    retired=True
    try:
        calm=0
        while calm<args.calm_seconds:
            if time.monotonic()>=min(until,started+CALM_SECONDS):
                raise GuardLimit('Calm admission deadline exceeded')
            records=unrelated(snapshot(),None)
            noisy=heavy(records)
            log.write(dict(monotonic_ns=time.monotonic_ns(),phase='before',heavy=noisy,processes=records))
            calm=0 if noisy else calm+1
            time.sleep(min(1,max(0,min(until,started+CALM_SECONDS)-time.monotonic())))
        directory=Path(tempfile.mkdtemp(prefix='pb-perf335-guard-'))
        gate=directory/'gate'
        registered=directory/'profile'
        env=dict(os.environ,PLAYBIT_PERFORMANCE_PROFILE=str(registered))
        if args.after_setup:
            env['PLAYBIT_PERFORMANCE_GATE']=str(gate)
        child=subprocess.Popen(command,env=env,start_new_session=True,umask=0o077)
        calm=0
        measuring=not args.after_setup
        while not exited(child):
            if time.monotonic()>=until or (cancel_until is not None and time.monotonic()>=cancel_until):
                raise GuardLimit('Performance operator or cancellation deadline exceeded')
            profile=None
            value=control_text(registered,1024)
            if value is not None:
                if (not value.startswith(('/private/tmp/pb-bench-','/private/tmp/pb-perf335-'))
                        or not value.endswith('/profile') or Path(value).resolve()!=Path(value)):
                    raise GuardLimit('Operator did not declare a dedicated performance profile')
                profile=Path(value)
            records=unrelated(snapshot(),child.pid,profile)
            noisy=heavy(records)
            if not measuring:
                gate_value=control_text(gate,5)
                if gate_value is not None:
                    if gate_value!='ready':
                        raise GuardLimit('Unknown performance measurement gate state')
                    calm=0 if noisy else calm+1
                    if calm>=args.calm_seconds:
                        permit_measurement(gate)
                        measuring=True
            log.write(dict(monotonic_ns=time.monotonic_ns(),phase='during' if measuring else 'prepare',
                           heavy=noisy,processes=records))
            if measuring and noisy and not contaminated:
                contaminated=True
                cancel_until=time.monotonic()+CANCEL_SECONDS
                print('Heavy concurrent activity: cancelling this cohort; partial evidence retained.',flush=True)
                child.send_signal(signal.SIGINT)
            time.sleep(min(1,max(0,until-time.monotonic())))
    except (OSError,ValueError,TypeError,RuntimeError,TimeoutError,subprocess.SubprocessError,KeyboardInterrupt) as error:
        failure=(str(error) or type(error).__name__)[:512]
    finally:
        if child is not None:
            try:
                finish_operator(child,cancel_until)
            except (OSError,ValueError,TypeError,RuntimeError,subprocess.SubprocessError) as error:
                retired=False
                failure='Owned operator retirement is uncertain: '+type(error).__name__
        if directory is not None and retired:
            try:
                # The protocol owns exactly two files, never a recursive tree.
                for name in ('gate','profile'):
                    (directory/name).unlink(missing_ok=True)
                directory.rmdir()
            except OSError:
                failure='Performance control directory needs inspection'
        result=dict(phase='result',contaminated=contaminated,child_exit=child.returncode if child else None)
        if directory is not None and directory.exists():
            result['control_directory']=str(directory)
        if failure is not None:
            result['failure']=failure
        log.write(result,final=True)
    return 75 if failure is not None or contaminated else child.returncode


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--log", type=Path, required=True)
    parser.add_argument("--after-setup", action="store_true",
                        help="Wait for the operator's measurement gate after fixture preparation")
    parser.add_argument("--calm-seconds", type=int, default=5,
                        help="Consecutive calm readings required before measuring (5..60)")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("expected an operator command after --")
    if not 5 <= args.calm_seconds <= 60:
        parser.error("calm-seconds must be between 5 and 60")
    if sys.platform not in ('darwin','linux'):
        parser.error('performance observation requires macOS or Linux')
    log=Log(args.log)
    def stop(*_):
        raise GuardLimit('Performance guard received shutdown')
    previous=signal.signal(signal.SIGTERM,stop)
    try:
        return run(args,command,log)
    finally:
        try:
            log.close()
        finally:
            signal.signal(signal.SIGTERM,previous)


if __name__ == "__main__":
    raise SystemExit(main())
