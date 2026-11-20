from pathlib import Path
import os
import sys
import subprocess
import select
import tempfile
import unittest

from process_capture import capture, departed_group, exited, retire


class ControlProcess(unittest.TestCase):
    def test_start_observation_identifies_the_actual_child_and_not_a_failed_spawn(self):
        observed=[]
        code,raw=capture([sys.executable,'-I','-c','import os; print(os.getpid())'],
                         seconds=5,limit=100,started=observed.append)
        self.assertEqual(code,0)
        self.assertEqual(observed,[int(raw)])
        with self.assertRaises(FileNotFoundError):
            capture(['/nonexistent-playbit-command'],seconds=5,limit=100,started=observed.append)
        self.assertEqual(observed,[int(raw)])

    def test_failed_start_report_retires_the_already_launched_child(self):
        observed=[]
        def failed(pid):
            observed.append(pid)
            raise RuntimeError('Progress persistence failed')
        with self.assertRaisesRegex(RuntimeError,'Progress persistence failed'):
            capture([sys.executable,'-I','-c','import time; time.sleep(30)'],
                    seconds=5,limit=100,started=failed)
        with self.assertRaises(ProcessLookupError):os.kill(observed[0],0)

    def test_compiler_stderr_is_retained_from_the_selected_source_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary)/'broken.rs'
            source.write_text('synthetic compiler fixture')
            command = [sys.executable,'-c',
                'from pathlib import Path;import sys;assert Path("broken.rs").is_file();'
                'sys.stderr.write("error[E0308]: mismatched types\\n");sys.exit(1)']
            self.assertEqual(capture(command,seconds=5,limit=4096,cwd=temporary,merge_stderr=True),
                (1,b'error[E0308]: mismatched types\n'))

    def test_exit_observation_retains_unreaped_group_identity_until_retirement(self):
        process=subprocess.Popen([sys.executable,'-c','import sys; print("ready",flush=True); sys.stdin.buffer.read(1)'],
                                 stdin=subprocess.PIPE,stdout=subprocess.PIPE,start_new_session=True)
        try:
            self.assertTrue(select.select([process.stdout],[],[],5)[0])
            self.assertEqual(process.stdout.readline().strip(),b'ready')
            self.assertFalse(exited(process))
            if sys.platform=='darwin':
                queue=select.kqueue()
                event=select.kevent(process.pid,filter=select.KQ_FILTER_PROC,
                                   flags=select.KQ_EV_ADD|select.KQ_EV_ONESHOT,fflags=select.KQ_NOTE_EXIT)
                queue.control([event],0,0)
                process.stdin.write(b'x'); process.stdin.flush()
                self.assertEqual(len(queue.control(None,1,5)),1)
                queue.close()
            else:
                process.stdin.write(b'x'); process.stdin.flush()
                # Observe exit without consuming/reaping the Linux child.
                import os
                os.waitid(os.P_PID,process.pid,os.WEXITED|os.WNOWAIT)
            self.assertTrue(exited(process))
            self.assertIsNone(process.returncode)
            retire(process)
            self.assertEqual(process.returncode,0)
            with self.assertRaisesRegex(RuntimeError,'reaped before'): exited(process)
        finally:
            if process.poll() is None: process.kill()
            process.communicate(timeout=5)

    def test_control_input_is_finite_noninteractive_and_admitted_before_exec(self):
        # A file-backed request cannot deadlock with command output, and an
        # oversized request is rejected before any child is launched.
        command=[sys.executable,'-c','import sys; sys.stdout.buffer.write(sys.stdin.buffer.read())']
        self.assertEqual(capture(command,seconds=5,limit=100,input_bytes=b'{"fixture":true}'),(0,b'{"fixture":true}'))
        self.assertEqual(capture(command,seconds=5,limit=100),(0,b''))
        with tempfile.TemporaryDirectory() as temporary:
            marker=Path(temporary)/'not-admitted'
            command=[sys.executable,'-c',f'from pathlib import Path; Path({str(marker)!r}).touch()']
            with self.assertRaisesRegex(ValueError,'input exceeds'):
                capture(command,seconds=5,limit=100,input_bytes=b'x'*8193)
            self.assertFalse(marker.exists())

    def test_failed_capture_retires_children_that_inherit_process_pipes(self):
        # The child's file lease witnesses actual lifetime. On a byte-limit
        # failure the whole owned group must retire, not just the immediate CLI.
        with tempfile.TemporaryDirectory() as temporary:
            path=Path(temporary)/'child.lock'
            child=('import fcntl,time; from pathlib import Path; '
                   f'f=open({str(path)!r},"w"); fcntl.flock(f,fcntl.LOCK_EX); '
                   'print("ready",flush=True); time.sleep(5)')
            parent=('import subprocess,sys; '
                    f'p=subprocess.Popen([sys.executable,"-c",{child!r}],stdout=subprocess.PIPE); '
                    'p.stdout.readline(); print("over the byte limit",flush=True); p.wait()')
            transcript=Path(temporary)/'partial.log'
            with transcript.open('xb') as output:
                with self.assertRaisesRegex(ValueError,'byte limit'):
                    capture([sys.executable,'-c',parent],seconds=5,limit=1,transcript=output)
            self.assertEqual(transcript.read_bytes(),b'o')
            observer=f'import fcntl; f=open({str(path)!r},"rb"); fcntl.flock(f,fcntl.LOCK_EX)'
            subprocess.run([sys.executable,'-c',observer],check=True,capture_output=True,timeout=2)

    @unittest.skipUnless(sys.platform=='darwin','macOS unreaped-group behavior')
    def test_live_group_is_distinct_from_its_unreaped_exited_leader(self):
        # NOTE_EXIT observes termination without reaping the leader, preserving
        # its identity while exercising macOS's EPERM cleanup case deterministically.
        process=subprocess.Popen([sys.executable,'-c','import os,sys; sys.stdin.buffer.read(1); os._exit(0)'],
                                 stdin=subprocess.PIPE,stdout=subprocess.PIPE,start_new_session=True)
        queue=select.kqueue()
        try:
            self.assertFalse(departed_group(process.pid))
            event=select.kevent(process.pid,filter=select.KQ_FILTER_PROC,
                               flags=select.KQ_EV_ADD|select.KQ_EV_ONESHOT,fflags=select.KQ_NOTE_EXIT)
            queue.control([event],0,0)
            process.stdin.write(b'x'); process.stdin.flush()
            events=queue.control(None,1,5)
            self.assertEqual(len(events),1)
            self.assertTrue(events[0].fflags&select.KQ_NOTE_EXIT)
            self.assertTrue(departed_group(process.pid))
            retire(process)
            self.assertEqual(process.returncode,0)
        finally:
            queue.close()
            if process.poll() is None:
                process.kill()
            process.communicate(timeout=5)


if __name__=='__main__':
    unittest.main()
