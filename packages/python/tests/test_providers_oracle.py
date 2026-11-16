import asyncio
import http.server
import json
import threading
import unittest
from hive_sdk import Agent, Objective, Oracle, PresetProvider


class Fake:
    """Local HTTP server; `handler(index, request_body)` returns (status, json)."""

    def __init__(self, handler):
        self.requests = []
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(inner):
                length = int(inner.headers.get('content-length', 0))
                body = json.loads(inner.rfile.read(length) or b'null')
                outer.requests.append({'headers': {k.lower(): v for k, v in inner.headers.items()}, 'path': inner.path, 'body': body})
                status, payload = handler(len(outer.requests), body)
                raw = json.dumps(payload).encode()
                inner.send_response(status)
                inner.send_header('content-type', 'application/json')
                inner.send_header('content-length', str(len(raw)))
                inner.end_headers()
                inner.wfile.write(raw)

            def log_message(inner, *_):
                pass
        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.base = f'http://127.0.0.1:{self.server.server_port}'

    def close(self):
        self.server.shutdown()
        self.server.server_close()


QUESTIONS = {'sure': {'type': 'noul', 'instructions': 'done?'}}


class ProviderPresetTests(unittest.IsolatedAsyncioTestCase):
    async def test_presets_expand_and_apply_their_wire_profile(self):
        fake = Fake(lambda n, body: (200, {'choices': [{'finish_reason': 'tool_calls', 'message': {'content': None, 'tool_calls': [
            {'id': 'c', 'type': 'function', 'function': {'name': 'my__tool', 'arguments': '{}'}}]}}],
            'usage': {'prompt_tokens': 3, 'completion_tokens': 1}}) if n == 1 else (200, {'choices': [{'finish_reason': 'stop',
            'message': {'content': 'done'}}], 'usage': {'prompt_tokens': 5, 'completion_tokens': 1}}))
        ran = []
        try:
            tool_def = {'name': 'my.tool', 'description': 'd', 'parameters': {'type': 'object', 'additionalProperties': False,
                        'properties': {'list': {'type': 'array'}}}}
            from hive_sdk import Tool
            agent = Agent(model='m', provider=PresetProvider('kimi', api_key='k', base_url=f'{fake.base}/v1'),
                          tools=[Tool(**tool_def, execute=lambda args: ran.append('ran') or 1)], max_turns=4)
            await agent.run('go')
            agent.close()
        finally:
            fake.close()
        self.assertEqual(ran, ['ran'])
        body = fake.requests[0]['body']
        self.assertEqual(fake.requests[0]['headers']['authorization'], 'Bearer k')
        self.assertEqual(body['tools'][0]['function']['name'], 'my__tool')
        self.assertIs(body['parallel_tool_calls'], False)
        self.assertEqual(body['tool_choice'], 'auto')
        self.assertEqual(body['tools'][0]['function']['parameters']['properties']['list']['items'], {})
        with self.assertRaisesRegex(Exception, 'unknown provider preset'):
            PresetProvider('nope')
        with self.assertRaisesRegex(Exception, 'OpenAI-compatible'):
            PresetProvider('anthropic', profile={'normalize_tool_names': True})


class OracleTests(unittest.IsolatedAsyncioTestCase):
    async def test_answers_closed_questions_and_falls_back_instead_of_raising(self):
        answers = {'next': {'type': 'choice', 'choice': 'finish', 'confidence': 0.9, 'probabilities': {'finish': 0.9, 'continue': 0.1}},
                   'sure': {'type': 'noul', 'noul': 0.7}}
        fake = Fake(lambda n, body: (200, {'answers': answers, 'usage': {'input_tokens': 40, 'output_tokens': 2}}) if n == 1 else (500, {}))
        questions = {'next': {'type': 'choice', 'instructions': 'what next', 'criteria': {'finish': 'done', 'continue': 'more'}}, **QUESTIONS}
        try:
            oracle = Oracle({'endpoint': f'{fake.base}/decisions', 'model': 'jev', 'api_key': 'secret', 'cooldown_ms': 10000})
            reply = await oracle.ask({'turn': 1}, questions)
            self.assertEqual((reply['outcome'], reply['answers']['next']['choice']), ('decision', 'finish'))
            self.assertEqual(reply['charge'], {'input_tokens': 40, 'output_tokens': 2})
            self.assertEqual(fake.requests[0]['headers']['authorization'], 'Bearer secret')
            self.assertEqual(fake.requests[0]['body']['state'], {'turn': 1})
            failed = await oracle.ask({}, questions)
            self.assertEqual((failed['outcome'], failed['reason'], failed['sent']), ('fallback', 'oracle HTTP 500', True))
            self.assertEqual(oracle.status()['state'], 'fallback')
            self.assertFalse(oracle.record_overruled('x'))
            oracle.record_agreement(); oracle.reset()
            self.assertEqual(oracle.status()['state'], 'ready')
            with self.assertRaisesRegex(Exception, 'endpoint'):
                Oracle({'endpoint': 'ftp://x', 'model': 'm'})
            with self.assertRaisesRegex(Exception, 'two criteria'):
                await oracle.ask({}, {'q': {'type': 'choice', 'instructions': 'i', 'criteria': {'only': 'one'}}})
        finally:
            fake.close()

    async def test_calls_are_reserved_on_and_settled_against_the_root_objective(self):
        fake = Fake(lambda n, body: (200, {'answers': {'sure': {'type': 'noul', 'noul': 0.5}}, 'usage': {'input_tokens': 25, 'output_tokens': 2}}))
        try:
            oracle = Oracle({'endpoint': f'{fake.base}/d', 'model': 'jev', 'max_output_tokens': 8})
            objective = Objective({'max_model_calls': 2, 'max_output_tokens': 50, 'max_total_output_tokens': 50})
            self.assertEqual((await oracle.ask({}, QUESTIONS, objective=objective))['outcome'], 'decision')
            snapshot = objective.inspect()
            self.assertEqual((snapshot['model_calls'], snapshot['input_tokens'], snapshot['output_tokens']), (1, 25, 2))
            await oracle.ask({}, QUESTIONS, objective=objective)
            with self.assertRaisesRegex(Exception, 'model_call_limit'):
                await oracle.ask({}, QUESTIONS, objective=objective)
        finally:
            fake.close()


if __name__ == '__main__':
    unittest.main()
