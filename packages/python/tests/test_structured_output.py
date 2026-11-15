import asyncio
from contextlib import asynccontextmanager
import json
import unittest
from hive_sdk import Agent, OpenAICompatible, Anthropic, Gemini
from test_agent import Scripted
from test_gemini import send_json
from test_streaming import headers, data

SCHEMA = {'type': 'object', 'properties': {'answer': {'type': 'string'}}, 'required': ['answer'], 'additionalProperties': False}
OUTPUT = {'name': 'answer_v1', 'schema': SCHEMA}
POLICY = {'structured_output': OUTPUT, 'max_output_tokens': 8, 'max_total_output_tokens': 20}
TEXT = ' \n{"answer":"sí 🦀"}\t'
USAGE = {'input_tokens': 3, 'output_tokens': 2}


def payload(kind, text):
    if kind == 'openai':
        return {'choices': [{'message': {'content': text}, 'finish_reason': 'stop'}], 'usage': {'prompt_tokens': 3, 'completion_tokens': 2}}
    if kind == 'anthropic':
        return {'type': 'message', 'role': 'assistant', 'content': [{'type': 'text', 'text': text}], 'stop_reason': 'end_turn', 'usage': USAGE}
    return {'candidates': [{'index': 0, 'content': {'role': 'model', 'parts': [{'text': text}]}, 'finishReason': 'STOP'}], 'usageMetadata': {'promptTokenCount': 3, 'candidatesTokenCount': 2}}


async def send(writer, kind, text, streaming):
    if not streaming:
        await send_json(writer, payload(kind, text)); return
    await headers(writer)
    chunks = [text[:3], text[3:]]
    if kind == 'openai':
        for part in chunks: await data(writer, {'choices': [{'index': 0, 'delta': {'content': part}}]})
        await data(writer, {'choices': [{'index': 0, 'delta': {}, 'finish_reason': 'stop'}], 'usage': {'prompt_tokens': 3, 'completion_tokens': 2}})
        writer.write(b'data: [DONE]\n\n'); await writer.drain()
    elif kind == 'anthropic':
        await data(writer, {'type': 'message_start', 'message': {'type': 'message', 'role': 'assistant', 'content': [], 'stop_reason': None, 'usage': {'input_tokens': 3, 'output_tokens': 0}}})
        await data(writer, {'type': 'content_block_start', 'index': 0, 'content_block': {'type': 'text', 'text': ''}})
        for part in chunks: await data(writer, {'type': 'content_block_delta', 'index': 0, 'delta': {'type': 'text_delta', 'text': part}})
        await data(writer, {'type': 'content_block_stop', 'index': 0})
        await data(writer, {'type': 'message_delta', 'delta': {'stop_reason': 'end_turn'}, 'usage': {'output_tokens': 2}})
        await data(writer, {'type': 'message_stop'})
    else:
        for part in chunks: await data(writer, {'candidates': [{'index': 0, 'content': {'role': 'model', 'parts': [{'text': part}]}}]})
        await data(writer, {'candidates': [{'index': 0, 'finishReason': 'STOP'}], 'usageMetadata': {'promptTokenCount': 3, 'candidatesTokenCount': 2}})


@asynccontextmanager
async def serve(kind, text, streaming, reject=False):
    requests, errors, tasks = [], [], set()
    async def connected(reader, writer):
        task = asyncio.current_task(); tasks.add(task)
        try:
            header = await reader.readuntil(b'\r\n\r\n')
            length = next(int(line.split(b':', 1)[1]) for line in header.lower().split(b'\r\n') if line.startswith(b'content-length:'))
            body = json.loads(await reader.readexactly(length)); requests.append(body)
            route = header.split(b' ')[1].decode()
            expected = ('/v1beta/models/test:' + ('streamGenerateContent?alt=sse' if streaming else 'generateContent')) if kind == 'gemini' else '/v1/chat/completions' if kind == 'openai' else '/v1/messages'
            assert route == expected
            assert 'structured_output' not in body
            if kind == 'openai':
                assert body['response_format'] == {'type': 'json_schema', 'json_schema': {'name': 'answer_v1', 'schema': SCHEMA, 'strict': True}}
                assert body['max_completion_tokens'] == 8
            elif kind == 'anthropic':
                assert body['output_config'] == {'format': {'type': 'json_schema', 'schema': SCHEMA}}
                assert body['max_tokens'] == 8
            else:
                assert body['generationConfig'] == {'candidateCount': 1, 'maxOutputTokens': 8, 'responseMimeType': 'application/json', 'responseJsonSchema': SCHEMA}
            if reject: await send_json(writer, {'error': 'unsupported schema'}, status=400)
            else: await send(writer, kind, text, streaming)
        except Exception as error: errors.append(error)
        finally:
            writer.close()
            try: await writer.wait_closed()
            except ConnectionError: pass
            tasks.discard(task)
    server = await asyncio.start_server(connected, '127.0.0.1', 0)
    port = server.sockets[0].getsockname()[1]
    provider_type = {'openai': OpenAICompatible, 'anthropic': Anthropic, 'gemini': Gemini}[kind]
    provider = provider_type(base_url=f'http://127.0.0.1:{port}/' + ('v1beta' if kind == 'gemini' else 'v1'), api_key='test-only')
    try: yield provider, requests
    finally:
        server.close(); await server.wait_closed()
        pending = list(tasks)
        for task in pending: task.cancel()
        if pending: await asyncio.gather(*pending, return_exceptions=True)
        if errors: raise errors[0]


class StructuredOutputTests(unittest.IsolatedAsyncioTestCase):
    async def roundtrip(self, streaming):
        for kind in ('openai', 'anthropic', 'gemini'):
            for content in (TEXT, '{"answer":7}'):
                with self.subTest(kind=kind, content=content):
                    async with serve(kind, content, streaming) as (provider, requests):
                        agent = Agent(model='test', provider=provider, policy=POLICY)
                        partial = ''
                        if streaming:
                            async for event in agent.stream('Answer'):
                                if event['type'] == 'text_delta': partial += event['text']
                                if event['type'] == 'result': result = event['result']
                            self.assertEqual(partial, content)
                        else: result = await agent.run('Answer')
                        if content == TEXT: self.assertEqual(result['content'], content)
                        else: self.assertEqual(result['stop_reason'], 'invalid_structured_output')
                        self.assertEqual(agent.messages[-1]['content'], content)
                        self.assertEqual(len(requests), 1); self.assertEqual(result['stats']['model_calls'], 1)
                        self.assertEqual(result['stats']['input_tokens'], 3); self.assertEqual(result['stats']['output_tokens'], 2)
                        self.assertEqual(result['stats']['reserved_output_tokens'], 0)

    async def test_json_preserves_schema_raw_text_and_known_usage_without_repair(self):
        await self.roundtrip(False)

    async def test_sse_validates_final_reply_instead_of_partial_json(self):
        await self.roundtrip(True)

    async def test_unsupported_provider_schemas_do_not_fallback(self):
        for kind in ('openai', 'anthropic', 'gemini'):
            async with serve(kind, TEXT, False, reject=True) as (provider, requests):
                with self.assertRaisesRegex(RuntimeError, 'PROVIDER_ERROR'):
                    await Agent(model='test', provider=provider, policy=POLICY).run('Answer')
                self.assertEqual(len(requests), 1)

    async def test_request_mutation_cannot_weaken_validation(self):
        requests = []
        def complete(request):
            requests.append(True); self.assertEqual(request['structured_output'], OUTPUT)
            request['structured_output']['schema'] = {}
            return {'content': '{}', 'usage': USAGE}
        result = await Agent(model='test', policy=POLICY, provider=Scripted(complete)).run('Answer')
        self.assertEqual(result['stop_reason'], 'invalid_structured_output'); self.assertEqual(len(requests), 1)

    async def test_invalid_schema_fails_before_callback(self):
        def complete(request): self.fail('no callback for invalid schema')
        with self.assertRaisesRegex(RuntimeError, 'INVALID_INPUT'):
            Agent(model='test', policy={'structured_output': {'name': 'bad.name', 'schema': SCHEMA}}, provider=Scripted(complete))
