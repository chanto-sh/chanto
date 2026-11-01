import queue
import unittest
from unittest.mock import Mock, patch
import os
import tempfile
from chanto_sdk import Chanto, ChantoError, Session, tool


class ToolDispatchTests(unittest.TestCase):
    def session(self, tools):
        with patch.object(Session, '_read_events'), patch('threading.Event.wait'):
            return Session(Mock(), 's1', tools=tools)

    def test_tool_dispatch(self):
        @tool('lookup', 'Lookup', {'type': 'object'})
        def lookup(args):
            return {'ticket': args['id']}
        s = self.session([lookup])
        s._events.put({'type': 'tool_call', 'id': 'c1', 'name': 'lookup', 'input': {'id': 42}})
        s._events.put({'type': 'turn_done', 'result': 'done'})
        self.assertEqual(s.run('lookup ticket'), 'done')
        s.client._request.assert_any_call('POST', '/sessions/s1/respond',
            {'id': 'c1', 'result': {'content': '{"ticket": 42}', 'is_error': False}})

    def test_handler_exception(self):
        @tool('fail', 'Fail', {'type': 'object'})
        def fail(args):
            raise ValueError('bad ticket')
        s = self.session([fail])
        s._events.put({'type': 'tool_call', 'id': 'c2', 'name': 'fail', 'input': {}})
        s._events.put({'type': 'turn_done', 'result': 'done'})
        s.run('fail')
        s.client._request.assert_any_call('POST', '/sessions/s1/respond',
            {'id': 'c2', 'result': {'content': 'bad ticket', 'is_error': True}})

    def test_fork_preserves_callbacks_and_tools(self):
        s = self.session([])
        s.client._request.return_value = {'id': 'child'}
        with patch('chanto_sdk.Session') as child:
            self.assertIs(s.fork(name='branch', point=2), child.return_value)
        s.client._request.assert_called_once_with('POST', '/sessions/s1/fork',
                                                 {'name': 'branch', 'point': 2})
        self.assertEqual(child.call_args.args[1], 'child')
        self.assertIs(child.call_args.kwargs['on_permission'], s.on_permission)

    def test_checkpoints_and_rewind(self):
        s = self.session([])
        s.checkpoints()
        s.client._request.assert_called_with('GET', '/sessions/s1/checkpoints')
        s.rewind(2, mode='code', preview=True)
        s.client._request.assert_called_with('POST', '/sessions/s1/rewind', {'checkpoint': 2, 'mode': 'code', 'preview': True})
        s.undo_rewind()
        s.client._request.assert_called_with('POST', '/sessions/s1/rewind', {'undo': True})

    def test_options_and_schema(self):
        h = Chanto('http://fake', 'token')
        h._request = Mock(return_value={'id': 's'})
        with patch('chanto_sdk.Session'):
            h.session(verify=False, allowed_tools=[], max_turns=2,
                tools=[tool('lookup', 'Lookup', {'type': 'object'})(lambda args: 'ok')])
        body = h._request.call_args.args[2]
        self.assertEqual(body['allowed_tools'], [])
        self.assertFalse(body['verify'])
        self.assertEqual(body['tools'][0]['input_schema'], {'type': 'object'})
        self.assertNotIn('handler', body['tools'][0])


class RunHelperTests(unittest.TestCase):
    def test_run_requests(self):
        h = Chanto('http://fake', 'token')
        h._request = Mock(return_value={'id': 'r1', 'output': 'Applied', 'runs': []})
        h.start_run('fix it', cwd='/repo', verify=False, max_cost_usd=1.5)
        h._request.assert_called_with('POST', '/runs', {'task': 'fix it', 'cwd': '/repo', 'verify': False, 'max_cost_usd': 1.5})
        h.respond_run('r1', decision='allow')
        h._request.assert_called_with('POST', '/runs/r1/respond', {'decision': 'allow'})
        self.assertEqual(h.apply_run('r1', pr=True), 'Applied')
        h._request.assert_called_with('POST', '/runs/r1/apply', {'pr': True})
        self.assertEqual(h.runs(), [])


class StartTests(unittest.TestCase):
    def fake_server(self, version, tmp):
        """A `chanto` that serves /health on a free port and says where."""
        import stat, sys, textwrap
        path = os.path.join(tmp, 'chanto')
        with open(path, 'w') as f:
            f.write(f'#!{sys.executable}\n' + textwrap.dedent(f"""
                import http.server, json, sys
                class H(http.server.BaseHTTPRequestHandler):
                    def log_message(self, *a): pass
                    def do_GET(self):
                        b = json.dumps({{'ok': True, 'version': '{version}'}}).encode()
                        self.send_response(200); self.send_header('Content-Length', str(len(b))); self.end_headers(); self.wfile.write(b)
                s = http.server.HTTPServer(('127.0.0.1', 0), H)
                print('chanto server listening on http://127.0.0.1:%d' % s.server_port, file=sys.stderr, flush=True)
                s.serve_forever()
            """))
        os.chmod(path, os.stat(path).st_mode | stat.S_IEXEC)
        return path

    @unittest.skipIf(os.name == 'nt', 'runs a script directly')
    def test_start_connects_and_close_stops_it(self):
        with tempfile.TemporaryDirectory() as tmp:
            with Chanto.start(self.fake_server('9.9.9', tmp)) as client:
                self.assertEqual(client.check_version(), '9.9.9')
                process = client._process
            self.assertIsNotNone(process.poll())
            client.close()

    @unittest.skipIf(os.name == 'nt', 'runs a script directly')
    def test_start_refuses_an_old_server_and_stops_it(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaisesRegex(ChantoError, 'chanto upgrade'):
                Chanto.start(self.fake_server('0.0.1', tmp))

    def test_start_says_when_the_binary_is_missing(self):
        with self.assertRaisesRegex(ChantoError, "couldn't start"):
            Chanto.start('/no/such/chanto')

    def test_connect_says_when_no_server_answers(self):
        with self.assertRaisesRegex(ChantoError, 'no Chanto server'):
            Chanto.connect('http://127.0.0.1:9', 't')

    def test_a_version_it_cant_read_is_allowed(self):
        client = Chanto('http://x', 't')
        with patch.object(client, 'health', return_value={'version': 'dev'}):
            self.assertEqual(client.check_version(), 'dev')


if __name__ == '__main__':
    unittest.main()
