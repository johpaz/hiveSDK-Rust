"""Deterministic stdio peer for SDK tests. Never contacts external services."""
import json
import os
import sys
import time

mode = sys.argv[1] if len(sys.argv) > 1 else 'normal'
receipt = sys.argv[2] if len(sys.argv) > 2 else None


def send(value):
    print(json.dumps(value, ensure_ascii=False, separators=(',', ':')), flush=True)


def response(id, result):
    send({'jsonrpc': '2.0', 'id': id, 'result': result})


def tool(name):
    schema = ({'type': 'object', 'properties': {'a': {'type': 'integer'}, 'b': {'type': 'integer'}},
               'required': ['a', 'b'], 'additionalProperties': False} if name == 'add'
              else {'type': 'object', 'properties': {'value': {}}, 'required': ['value']})
    value = {'name': name, 'description': 'Add integers' if name == 'add' else 'Echo JSON', 'inputSchema': schema,
             'annotations': {'readOnlyHint': True}}
    if name == 'add': value['outputSchema'] = {'type': 'object', 'properties': {'answer': {'type': 'integer'}}, 'required': ['answer']}
    return value


for raw in sys.stdin:
    request = json.loads(raw)
    method = request.get('method')
    id = request.get('id')
    if method == 'initialize':
        assert request['params']['capabilities'] == {}
        if mode == 'noisy_stderr': sys.stderr.write('stderr is not protocol\n' * 100000); sys.stderr.flush()
        response(id, {'protocolVersion': '2026-07-28' if mode == 'wrong_version' else request['params']['protocolVersion'],
            'capabilities': ({} if mode == 'no_tools' else {'resources': {}, 'prompts': {}} if mode in ('catalog', 'bad_resource') else {'tools': {'listChanged': True}}),
            'serverInfo': {'name': 'hive-test-peer', 'version': '1'},
            'instructions': 'Do not inject this string into a system prompt.'})
    elif method == 'notifications/initialized':
        pass
    elif method == 'resources/list':
        response(id, {'resources': [{'uri': 'test://guide', 'name': 'Guide'}], 'nextCursor': 'second'} if 'cursor' not in request['params'] else {'resources': []})
    elif method == 'resources/read':
        response(id, {'contents': [{'uri': request['params']['uri'], 'text': 'Use the SDK'}]} if mode != 'bad_resource' else {'contents': [{'text': 'missing uri'}]})
    elif method == 'prompts/list':
        response(id, {'prompts': [{'name': 'review', 'arguments': [{'name': 'topic', 'required': True}]}]})
    elif method == 'prompts/get':
        response(id, {'description': 'Review topic', 'messages': [{'role': 'user', 'content': {'type': 'text', 'text': 'Review ' + request['params'].get('arguments', {}).get('topic', '')}}]})
    elif method == 'tools/list':
        if mode == 'bad_schema':
            value = tool('add'); value['inputSchema']['required'] = True; response(id, {'tools': [value]})
        elif mode == 'external_schema':
            value = tool('add'); value['inputSchema']['$ref'] = 'https://example.invalid/schema'; response(id, {'tools': [value]})
        elif mode == 'duplicate': response(id, {'tools': [tool('add'), tool('add')]})
        elif mode == 'cycle': response(id, {'tools': [tool('add')] if 'cursor' not in request['params'] else [], 'nextCursor': 'again'})
        elif mode == 'pages' and 'cursor' not in request['params']: response(id, {'tools': [tool('add')], 'nextCursor': 'next'})
        elif mode == 'pages': response(id, {'tools': [tool('echo')]})
        else: response(id, {'tools': [tool('add'), tool('echo')]})
    elif method == 'tools/call':
        params = request['params']
        if receipt:
            with open(receipt, 'a') as output:
                output.write(json.dumps({'name': params['name'], 'arguments': params['arguments'],
                    'meta': params.get('_meta'), 'pid': os.getpid()}) + '\n')
                output.flush()
        if mode == 'exit': os._exit(0)
        if mode == 'wait':
            while True: time.sleep(60)
        if mode == 'invalid_json': print('{', flush=True); continue
        if mode == 'wrong_id': response(id + 100, {'content': []}); continue
        if mode == 'bad_result': response(id, {'content': [42]}); continue
        if mode == 'flood':
            for _ in range(10): send({'jsonrpc': '2.0', 'method': 'notifications/message', 'params': {}})
        if mode == 'server_requests':
            send({'jsonrpc': '2.0', 'id': 'server-ping', 'method': 'ping'})
            assert json.loads(sys.stdin.readline()) == {'jsonrpc': '2.0', 'id': 'server-ping', 'result': {}}
            send({'jsonrpc': '2.0', 'id': 'server-sampling', 'method': 'sampling/createMessage', 'params': {}})
            denied = json.loads(sys.stdin.readline())
            assert denied['id'] == 'server-sampling' and denied['error']['code'] == -32601
        if mode == 'remote_error':
            send({'jsonrpc': '2.0', 'id': id, 'error': {'code': -32602, 'message': 'Known remote failure'}}); continue
        if mode == 'tool_error': response(id, {'content': [{'type': 'text', 'text': 'Known tool failure'}], 'isError': True}); continue
        if params['name'] == 'add':
            answer = params['arguments']['a'] + params['arguments']['b']
            value = {'content': [{'type': 'text', 'text': str(answer)}], 'structuredContent': {'answer': answer}}
            if mode == 'bad_output': value['structuredContent']['answer'] = 'invalid'
            if mode == 'env': value['structuredContent']['env_keys'] = sorted(os.environ)
        else: value = {'content': [{'type': 'text', 'text': json.dumps(params['arguments']['value'])}], 'structuredContent': {'value': params['arguments']['value']}}
        response(id, value)
