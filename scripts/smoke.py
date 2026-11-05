#!/usr/bin/env python3
"""End-to-end HTTP and no-source-write checks using synthetic fixtures only."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request


def snapshot(root):
    return {str(p.relative_to(root)): (p.stat().st_mtime_ns, hashlib.sha256(p.read_bytes()).hexdigest())
            for p in root.rglob('*') if p.is_file()}


def main():
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else 'target/debug/token-ledger').resolve()
    with tempfile.TemporaryDirectory(prefix='token-ledger-smoke-') as tmp:
        root = Path(tmp).resolve()
        home = root / 'fixture-home'
        source = home / '.codex/sessions'
        source.mkdir(parents=True)
        log = source / 'synthetic.jsonl'
        rows = [
            {'type': 'session_meta', 'payload': {'id': 'synthetic-session', 'cwd': '/synthetic/project'}},
            {'type': 'event_msg', 'timestamp': '2026-11-05T10:00:00Z', 'payload': {'type': 'token_count', 'info': {
                'total_token_usage': {'input_tokens': 100, 'cached_input_tokens': 80, 'output_tokens': 10},
                'last_token_usage': {'input_tokens': 100, 'cached_input_tokens': 80, 'output_tokens': 10, 'reasoning_output_tokens': 3}}}}
        ]
        log.write_text('\n'.join(json.dumps(r) for r in rows) + '\n')
        log.chmod(0o444)
        before = snapshot(home)
        proc = subprocess.Popen([str(binary), '--source-home', str(home), '--data-dir', str(root / 'data'), 'serve'],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            proc.stdout.readline()
            launch = proc.stdout.readline().strip()
            parsed = urllib.parse.urlsplit(launch)
            assert parsed.hostname == '127.0.0.1', 'Listener must be loopback only'
            base = f'{parsed.scheme}://{parsed.netloc}'
            token = urllib.parse.parse_qs(parsed.fragment)['token'][0]

            def request(path, *, auth=True, origin=None, method='GET', body=None, host=None):
                headers = {'Authorization': f'Bearer {token}'} if auth else {}
                if origin is not None:
                    headers['Origin'] = origin
                if host is not None:
                    headers['Host'] = host
                data = None
                if body is not None:
                    data = json.dumps(body).encode()
                    headers['Content-Type'] = 'application/json'
                req = urllib.request.Request(base + path, data=data, headers=headers, method=method)
                try:
                    response = urllib.request.urlopen(req, timeout=10)
                except urllib.error.HTTPError as error:
                    return error.code, json.loads(error.read())
                raw = response.read()
                return response.status, json.loads(raw) if 'application/json' in response.headers.get('Content-Type', '') else raw

            assert request('/api/summary', auth=False)[0] == 403
            assert request('/api/summary', host='rebind.invalid')[0] == 403
            assert request('/api/summary', origin='https://foreign.invalid')[0] == 403
            assert request('/api/scan', method='POST')[0] == 403
            for _ in range(100):
                coverage = request('/api/coverage')[1]
                if coverage['last_scan'] and not coverage['scanning']:
                    break
                time.sleep(.05)
            else:
                raise AssertionError('Initial scan did not complete')
            summary_path = '/api/summary?from=2026-11-05&to=2026-11-05'
            assert request(summary_path)[1]['total_tokens'] == 110
            series = request('/api/timeseries?from=2026-11-05&to=2026-11-05&grain=hour')[1]
            assert len(series['points']) == 24
            assert sum(p['total_tokens'] for p in series['points']) == 110
            assert request('/')[0] == 200
            assert request('/app.js')[0] == 200
            raw_event = [{'id': 'export-one', 'timestamp': '2026-11-05T10:00:00Z', 'model': 'synthetic',
                          'tokenUsage': {'inputTokens': 20, 'outputTokens': 2, 'cacheReadTokens': 100},
                          'prompt': 'PRIVATE-SYNTHETIC-TEXT'}]
            imported = {'format': 'json', 'app': 'cursor', 'account': 'test', 'from': '2026-11-05',
                        'to': '2026-11-05', 'name': 'synthetic.json', 'content': json.dumps(raw_event)}
            for _ in range(2):
                status, result = request('/api/import', origin=base, method='POST', body=imported)
                assert status == 200, result
            assert request(summary_path)[1]['total_tokens'] == 232
            exported = request('/api/export')[1]
            assert 'PRIVATE-SYNTHETIC-TEXT' not in json.dumps(exported)
            assert not any(b'PRIVATE-SYNTHETIC-TEXT' in p.read_bytes() for p in (root / 'data').iterdir() if p.is_file())
            assert request('/api/scan', origin=base, method='POST')[0] == 202
            assert snapshot(home) == before, 'Source files, modification times, and file inventory changed'
            print('PASS: HTTP authorization, Origin/Host, browser assets, import idempotence, time buckets, privacy, source immutability')
        finally:
            proc.terminate()
            proc.wait(timeout=10)


if __name__ == '__main__':
    main()
