import queue
import unittest
from unittest.mock import Mock, patch
from chanto_sdk import Chanto, Session, tool


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


if __name__ == '__main__':
    unittest.main()
