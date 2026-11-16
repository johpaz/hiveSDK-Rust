import asyncio
import http.server
import threading
import unittest
from hive_sdk import OpenAICompatible

OK = b'{"choices":[{"message":{"role":"assistant","content":"hola"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}'


def serve(script):
    hits = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            self.rfile.read(int(self.headers.get('Content-Length', 0)))
            hits.append(1)
            status = script(len(hits))
            body = OK if status == 200 else b'no'
            self.send_response(status)
            self.send_header('Content-Length', str(len(body)))
            self.send_header('Content-Type', 'application/json')
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args): pass
    httpd = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd, hits, f'http://127.0.0.1:{httpd.server_port}/v1'


REQUEST = {'model': 'm', 'messages': [{'role': 'user', 'content': 'hi'}], 'tools': []}


class ResilienceTests(unittest.IsolatedAsyncioTestCase):
    async def test_refused_requests_are_retried_ambiguous_failures_are_not(self):
        flaky, hits, url = serve(lambda n: 429 if n == 1 else 503 if n == 2 else 200)
        try:
            provider = OpenAICompatible(base_url=url, resilience={'retry': {'max_attempts': 3, 'base_delay_ms': 5, 'max_delay_ms': 20}})
            self.assertEqual((await provider.complete(REQUEST))['content'], 'hola')
            self.assertEqual(len(hits), 3)
        finally:
            flaky.shutdown(); flaky.server_close()
        broken, hits, url = serve(lambda n: 500)
        try:
            provider = OpenAICompatible(base_url=url, resilience={'retry': {'max_attempts': 4, 'base_delay_ms': 5, 'max_delay_ms': 20}})
            with self.assertRaisesRegex(Exception, 'HTTP 500'):
                await provider.complete(REQUEST)
            self.assertEqual(len(hits), 1)
        finally:
            broken.shutdown(); broken.server_close()

    async def test_breaker_and_validation(self):
        down, hits, url = serve(lambda n: 500)
        try:
            provider = OpenAICompatible(base_url=url, resilience={'circuit_breaker': {'failure_threshold': 2, 'cooldown_ms': 60000}})
            for _ in range(2):
                with self.assertRaisesRegex(Exception, 'HTTP 500'):
                    await provider.complete(REQUEST)
            with self.assertRaisesRegex(Exception, 'PROVIDER_CIRCUIT_OPEN'):
                await provider.complete(REQUEST)
            self.assertEqual(len(hits), 2)
        finally:
            down.shutdown(); down.server_close()
        with self.assertRaisesRegex(Exception, 'max_attempts'):
            OpenAICompatible(resilience={'retry': {'max_attempts': 9}})
        OpenAICompatible()


if __name__ == '__main__':
    unittest.main()
