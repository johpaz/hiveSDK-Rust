import asyncio
import http.server
import json
import threading
import unittest
from hive_sdk import Anthropic, Ollama


def _server(reply):
    seen = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args): pass
        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            seen.append((self.path, body))
            data = json.dumps(reply).encode()
            self.send_response(200); self.send_header('Content-Type', 'application/json'); self.send_header('Content-Length', str(len(data)))
            self.end_headers(); self.wfile.write(data)

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, seen


REQUEST = {'model': 'llama3.2', 'messages': [{'role': 'user', 'content': 'hola'}], 'tools': []}


class OllamaTests(unittest.IsolatedAsyncioTestCase):
    async def test_ollama_talks_to_api_chat_with_the_options_only_its_native_api_takes(self):
        server, seen = _server({'message': {'role': 'assistant', 'content': 'hola desde ollama'}, 'done': True, 'done_reason': 'stop', 'prompt_eval_count': 4, 'eval_count': 3})
        try:
            provider = Ollama(base_url=f'http://127.0.0.1:{server.server_address[1]}/api', num_ctx=16384, keep_alive='10m', think=False)
            reply = await provider.complete(REQUEST)
            self.assertEqual(reply['content'] if isinstance(reply, dict) else reply.content, 'hola desde ollama')
            path, body = seen[0]
            self.assertEqual(path, '/api/chat')
            self.assertEqual((body['options']['num_ctx'], body['options']['num_predict']), (16384, 2457))
            self.assertEqual((body['keep_alive'], body['think']), ('10m', False))
            with self.assertRaises(Exception): Ollama(num_ctx=0)
        finally: server.shutdown(); server.server_close()

    async def test_the_output_limit_is_derived_from_the_context_window_unless_one_is_given(self):
        server, seen = _server({'id': 'm', 'type': 'message', 'role': 'assistant', 'content': [{'type': 'text', 'text': 'ok'}], 'stop_reason': 'end_turn', 'usage': {'input_tokens': 1, 'output_tokens': 1}})
        try:
            base = dict(base_url=f'http://127.0.0.1:{server.server_address[1]}/v1', api_key='test-only')
            request = {**REQUEST, 'model': 'claude'}
            await Anthropic(**base, context_window=200_000).complete(request)
            await Anthropic(**base).complete(request)
            await Anthropic(**base, context_window=200_000, default_max_output_tokens=500).complete(request)
            self.assertEqual([body['max_tokens'] for _, body in seen], [30000, 4096, 500])
        finally: server.shutdown(); server.server_close()
