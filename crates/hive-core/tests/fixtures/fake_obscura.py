#!/usr/bin/env python3
"""A stand-in for `obscura mcp`, used only by tests.

It speaks the legacy MCP handshake Obscura 0.2.x uses (it answers `initialize`
with an older revision than requested), sends its own notifications and a `ping`,
and returns text, image and resource blocks. Every start and tool call is appended
to `<this file>.log` so tests can assert what was forwarded. Special URLs and
arguments trigger failures: `hang`, `crash`, `error`, `redir-bad`, `noaddr`.
"""
import base64
import json
import os
import struct
import sys
import time

LOG = os.environ.get('FAKE_OBSCURA_LOG') or (os.path.abspath(__file__) + '.log')
ARGV = sys.argv[1:]


def log(entry):
    with open(LOG, 'a') as handle:
        handle.write(json.dumps(entry) + '\n')


def send(message):
    sys.stdout.write(json.dumps(message) + '\n')
    sys.stdout.flush()


def png(width, height):
    header = b'\x89PNG\r\n\x1a\n' + struct.pack('>I', 13) + b'IHDR' + struct.pack('>II', width, height) + bytes([8, 6, 0, 0, 0]) + b'\0\0\0\0'
    return header


def text(value, error=False):
    result = {'content': [{'type': 'text', 'text': value}]}
    if error:
        result['isError'] = True
    return result


state = {'url': 'about:blank'}
log({'argv': ARGV})
for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    request = json.loads(raw)
    method, ident = request.get('method'), request.get('id')
    if method == 'initialize':
        revision = '1999-01-01' if 'bad-revision' in ARGV else '2024-11-05'
        send({'jsonrpc': '2.0', 'id': ident, 'result': {'protocolVersion': revision, 'capabilities': {'tools': {}}, 'serverInfo': {'name': 'fake-obscura', 'version': '0.0.0'}}})
        continue
    if ident is None:
        log({'notification': method})
        continue
    if 'id' in request and method is None:
        log({'client_reply': request})  # a reply to our ping
        continue
    if method != 'tools/call':
        send({'jsonrpc': '2.0', 'id': ident, 'error': {'code': -32601, 'message': 'unknown method'}})
        continue
    tool, arguments = request['params']['name'], request['params'].get('arguments', {})
    log({'tool': tool, 'arguments': arguments})
    # Noise a real server may send before answering.
    send({'jsonrpc': '2.0', 'method': 'notifications/message', 'params': {'level': 'info', 'data': 'working'}})
    send({'jsonrpc': '2.0', 'id': 'srv-1', 'method': 'ping'})
    result = None
    if tool == 'browser_navigate':
        url = arguments.get('url', '')
        if 'hang' in url:
            time.sleep(60)
        if 'crash' in url:
            sys.exit(3)
        if 'error' in url:
            result = text('navigation failed: DNS error', error=True)
        else:
            state['url'] = 'http://blocked.example/final' if 'redir-bad' in url else url
            result = text('URL: %s\nTitle: Fake page' % state['url'])
    elif tool == 'browser_evaluate':
        if arguments.get('expression') == 'location.href':
            result = text('"weird"' if 'noaddr' in state['url'] else json.dumps(state['url']))
        else:
            result = text(json.dumps({'echo': arguments.get('expression')}))
    elif tool == 'browser_close':
        state['url'] = 'about:blank'
        result = text('closed')
    elif tool == 'browser_snapshot':
        result = text('URL: %s\n[e1] link "Docs"\n' % state['url'] + 'palabra ' * 2000)
    elif tool == 'browser_screenshot':
        if arguments.get('width') == 999:
            result = text('screenshots are not supported by this build', error=True)
        else:
            data = png(arguments.get('width', 4), arguments.get('height', 3)) + os.urandom(200)
            result = {'content': [{'type': 'image', 'data': base64.b64encode(data).decode(), 'mimeType': 'image/png'}]}
    elif tool == 'browser_pdf':
        blob = b'%PDF-1.4\n% fake pdf for tests\n' + os.urandom(64)
        result = {'content': [{'type': 'text', 'text': 'exported'}, {'type': 'resource', 'resource': {'uri': 'file:///x.pdf', 'mimeType': 'application/pdf', 'blob': base64.b64encode(blob).decode()}}]}
    else:
        result = text('%s ok %s' % (tool, json.dumps(arguments, sort_keys=True)))
    if 'partial' in json.dumps(arguments):
        sys.stdout.write('this line is not JSON\n')
    send({'jsonrpc': '2.0', 'id': ident, 'result': result})
