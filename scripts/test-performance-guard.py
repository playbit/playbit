from pathlib import Path
import json
import fcntl
import os
import runpy
import signal
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest import mock

guard = runpy.run_path(str(Path(__file__).with_name("performance-guard.py")))


class GuardTests(unittest.TestCase):
    def test_excludes_operator_children_and_only_dedicated_reparented_gui(self):
        records = [dict(pid=pid, parent=parent, cpu=cpu, executable=executable)
                   for pid, parent, cpu, executable in [
                       (10, 1, 2, "python"), (11, 10, 3, "driver"), (12, 11, 4, "oracle"),
                       (13, 1, 150, "/private/tmp/pb-bench-xyz/profile/apps/a/Playbit"),
                       (14, 1, 200, "/work/build/rustc"),
                       (15, 1, 200, "/private/tmp/pb-bench-other/profile/apps/a/Playbit")]]
        result = guard["unrelated"](records, 10, Path("/private/tmp/pb-bench-xyz/profile"))
        self.assertEqual([r["pid"] for r in result], [14, 15])
        self.assertTrue(guard["heavy"](result))

    def test_detects_observed_jobs_and_parallel_load(self):
        self.assertTrue(guard["heavy"]([dict(cpu=101)]))
        self.assertTrue(guard["heavy"]([dict(cpu=51, executable="/usr/bin/tar")]))
        self.assertTrue(guard["heavy"]([dict(cpu=51, executable="/work/tool/target/debug/deps/diagnostic")]))
        self.assertTrue(guard["heavy"]([dict(cpu=90) for _ in range(5)]))
        self.assertFalse(guard["heavy"]([dict(cpu=60, executable="desktop focus work")]))
        self.assertFalse(guard["heavy"]([dict(cpu=20), dict(cpu=40)]))

    def test_prepared_operator_waits_for_calm_then_cancels_without_replay(self):
        class Child:
            pid = 987654
            returncode = -2
            polls = 0
            signals = []

            def poll(self):
                self.polls += 1
                return self.returncode if self.polls > 7 else None

            def send_signal(self, value):
                self.signals.append(value)

        child = Child()
        gate = None

        def start(command, env, start_new_session, umask):
            nonlocal gate
            self.assertEqual(command, ["operator"])
            self.assertTrue(start_new_session)
            self.assertEqual(umask,0o077)
            gate = Path(env["PLAYBIT_PERFORMANCE_GATE"])
            gate.write_text("ready")
            gate.chmod(0o600)
            return child

        def readings():
            for cpu in [0] * 5 + [150] + [0] * 5 + [150]:
                yield [dict(pid=123, parent=1, cpu=cpu, executable="unrelated")]

        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "activity.jsonl"
            with mock.patch.dict(guard["main"].__globals__, snapshot=mock.Mock(side_effect=readings()),
                                 exited=lambda p:p.poll() is not None,retire=mock.Mock()), \
                    mock.patch("sys.argv", ["guard", "--after-setup", "--log", str(output), "--", "operator"]), \
                    mock.patch("subprocess.Popen", side_effect=start), mock.patch("time.sleep"):
                self.assertEqual(guard["main"](), 75)
            records = list(map(json.loads, output.read_text().splitlines()))
            self.assertEqual(child.signals, [signal.SIGINT])
            self.assertTrue(records[5]["heavy"])
            self.assertEqual(records[5]["phase"], "prepare")
            self.assertEqual(records[-1], dict(phase="result", contaminated=True, child_exit=-2))

    def test_calm_deadline_refuses_to_launch_and_retains_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'log'
            log=guard['Log'](path)
            try:
                with mock.patch.dict(guard['run'].__globals__,CALM_SECONDS=0),mock.patch('subprocess.Popen') as launch:
                    self.assertEqual(guard['run'](SimpleNamespace(calm_seconds=5),['not-admitted'],log),75)
                    launch.assert_not_called()
            finally: log.close()
            record=json.loads(path.read_text())
            self.assertIn('Calm admission deadline',record['failure'])
            self.assertIsNone(record['child_exit'])

    def test_unresponsive_owned_operator_is_retired_within_deadline(self):
        # A real child ignores cooperative SIGINT. Its file lease proves actual
        # retirement after the deadline, rather than merely a sent kill signal.
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory); lease=root/'lifetime'; ready=root/'ready'
            code=('import fcntl,signal,time; from pathlib import Path; '
                  'signal.signal(signal.SIGINT,signal.SIG_IGN); '
                  f'f=open({str(lease)!r},"w"); fcntl.flock(f,fcntl.LOCK_EX); '
                  f'Path({str(ready)!r}).touch(); time.sleep(30)')
            log=guard['Log'](root/'log')
            sleep=time.sleep; started=time.monotonic()
            try:
                with mock.patch.dict(guard['run'].__globals__,MAX_SECONDS=0.5,CANCEL_SECONDS=0.05,
                                     CLEANUP_SECONDS=0.05,OBSERVATION_SECONDS=0.05,snapshot=lambda:[]), \
                        mock.patch('time.sleep',side_effect=lambda seconds:sleep(min(seconds,0.005))):
                    self.assertEqual(guard['run'](SimpleNamespace(calm_seconds=1,after_setup=False),[sys.executable,'-c',code],log),75)
            finally: log.close()
            self.assertLess(time.monotonic()-started,2)
            self.assertTrue(ready.exists())
            with lease.open('rb') as stream:
                fcntl.flock(stream,fcntl.LOCK_EX|fcntl.LOCK_NB)
            result=json.loads((root/'log').read_text().splitlines()[-1])
            self.assertEqual(result['child_exit'],-signal.SIGKILL)
            self.assertIn('deadline exceeded',result['failure'])

    def test_log_budget_reserves_terminal_failure_and_never_overwrites_a_prior_attempt(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'log'
            log=guard['Log'](path)
            try:
                with mock.patch.dict(guard['Log'].write.__globals__,MAX_LOG_BYTES=8192):
                    with self.assertRaisesRegex(guard['GuardLimit'],'log limit'):
                        log.write(dict(payload='x'*5000))
                    log.write(dict(phase='result',failure='fixture capacity exhausted'),final=True)
            finally: log.close()
            self.assertEqual(path.stat().st_mode&0o777,0o600)
            self.assertEqual(json.loads(path.read_text())['phase'],'result')
            with self.assertRaises(FileExistsError): guard['Log'](path)

    def test_control_files_and_observation_count_are_admitted_before_use(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory); path=root/'profile'
            path.write_bytes(b'x'*1025)
            with self.assertRaises(guard['GuardLimit']): guard['control_text'](path,1024)
            link=root/'link'; link.symlink_to(path)
            with self.assertRaises(OSError): guard['control_text'](link,1024)
            fifo=root/'fifo'; os.mkfifo(fifo)
            with self.assertRaises(guard['GuardLimit']): guard['control_text'](fifo,1024)
        with mock.patch.dict(guard['snapshot'].__globals__,MAX_PROCESSES=1,
                             capture=lambda *_args,**_kwargs:(0,b'1 0 0 fixture\n2 0 0 other\n')):
            with self.assertRaisesRegex(guard['GuardLimit'],'count limit'): guard['snapshot']()

    def test_shutdown_during_observation_retains_the_shutdown_reason(self):
        # Selector wrappers can swallow InterruptedError. Exercise the actual
        # capture path while this process owns the guard's SIGTERM handler.
        class Child:
            pid=987654
            returncode=None
            def send_signal(self,value): self.returncode=0
        child=Child(); observed=0
        launch=subprocess.Popen
        def start(command,**options):
            return child if command==['operator'] else launch(command,**options)
        def snapshot():
            nonlocal observed
            observed+=1
            if observed<=5: return []
            from process_capture import capture
            code='import os,signal,time; time.sleep(0.05); os.kill(os.getppid(),signal.SIGTERM); time.sleep(5)'
            capture([sys.executable,'-c',code],seconds=2,limit=100)
            raise AssertionError('shutdown observation returned without its failure')
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'log'
            with mock.patch.dict(guard['main'].__globals__,snapshot=snapshot,exited=lambda p:p.returncode is not None), \
                    mock.patch('sys.argv',['guard','--log',str(path),'--','operator']), \
                    mock.patch('subprocess.Popen',side_effect=start),mock.patch('time.sleep'):
                self.assertEqual(guard['main'](),75)
            self.assertIn('received shutdown',json.loads(path.read_text().splitlines()[-1])['failure'])


if __name__ == "__main__":
    unittest.main()
