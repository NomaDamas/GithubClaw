#!/usr/bin/env python3
from __future__ import annotations

import argparse
import dataclasses
import hashlib
import hmac
import json
import os
import pathlib
import random
import re
import shutil
import signal
import subprocess
import sys
import textwrap
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from typing import Any

ROOT = pathlib.Path(__file__).resolve().parents[1]
DEFAULT_SANDBOX_URL = os.environ.get(
    'PRODUCT_E2E_SANDBOX_URL',
    'git@github.com:vkehfdl1/GithubClaw-Sandbox.git',
)
DEFAULT_FIXTURE = ROOT / 'fixtures' / 'product-e2e' / 'bug-minimal.md'
DEFAULT_TIMEOUT_SECONDS = int(os.environ.get('PRODUCT_E2E_TIMEOUT', '180'))
DEFAULT_LOOP_WINDOW_SECONDS = int(os.environ.get('PRODUCT_E2E_LOOP_WINDOW', '45'))
DEFAULT_LOOP_THRESHOLD = int(os.environ.get('PRODUCT_E2E_LOOP_THRESHOLD', '4'))
DEFAULT_HEALTH_PORT = int(os.environ.get('PRODUCT_E2E_PORT', '8000'))
DEFAULT_E2E_ROOT = pathlib.Path(
    os.environ.get('PRODUCT_E2E_ROOT', ROOT / '.worktrees' / 'product-e2e-runs')
).expanduser()
DEFAULT_AUTO_RESTART = os.environ.get('PRODUCT_E2E_AUTO_RESTART', '1') not in {'0', 'false', 'False'}

SSH_RE = re.compile(r'^git@github\.com:([^/]+)/([^/]+?)(?:\.git)?$')
HTTPS_RE = re.compile(r'^https://github\.com/([^/]+)/([^/]+?)(?:\.git)?$')
NUMBER_RE = re.compile(r'(?<![A-Za-z])\d+(?![A-Za-z])')
WHITESPACE_RE = re.compile(r'\s+')


class HarnessError(RuntimeError):
    pass


@dataclasses.dataclass
class HarnessConfig:
    sandbox_url: str
    repo_full_name: str
    repo_key: str
    githubclaw_home: pathlib.Path
    shared_githubclaw_home: pathlib.Path
    checkout_dir: pathlib.Path
    fixture_path: pathlib.Path
    timeout_seconds: int
    loop_window_seconds: int
    loop_threshold: int
    port: int
    auto_restart: bool
    run_root: pathlib.Path

    @property
    def runtime_repo_dir(self) -> pathlib.Path:
        return self.githubclaw_home / 'runtime' / self.repo_key

    @property
    def queue_dir(self) -> pathlib.Path:
        return self.runtime_repo_dir / 'queue'

    @property
    def dispatch_receipts_dir(self) -> pathlib.Path:
        return self.runtime_repo_dir / 'dispatch_receipts'

    @property
    def sessions_repo_dir(self) -> pathlib.Path:
        return self.githubclaw_home / 'sessions' / self.repo_key

    @property
    def product_e2e_dir(self) -> pathlib.Path:
        return self.runtime_repo_dir / 'product-e2e'

    @property
    def repo_override_agents_dir(self) -> pathlib.Path:
        return self.githubclaw_home / 'repos' / self.repo_key / 'agents'

    @property
    def webhook_secret_path(self) -> pathlib.Path:
        return self.githubclaw_home / 'secrets' / 'webhook_secret'

    @property
    def shared_webhook_secret_path(self) -> pathlib.Path:
        return self.shared_githubclaw_home / 'secrets' / 'webhook_secret'

    @property
    def server_log_path(self) -> pathlib.Path:
        return self.githubclaw_home / 'logs' / 'webhook_server.log'

    @property
    def health_url(self) -> str:
        return f'http://127.0.0.1:{self.port}/health'


@dataclasses.dataclass
class RunArtifacts:
    run_id: str
    scenario: str
    backend: str
    run_dir: pathlib.Path
    raw_dir: pathlib.Path
    commands_log: pathlib.Path
    summary_path: pathlib.Path
    verdict_path: pathlib.Path
    summary_lines: list[str] = dataclasses.field(default_factory=list)
    first_failure: str | None = None

    def note(self, message: str) -> None:
        stamp = now_iso()
        line = f'[{stamp}] {message}'
        self.summary_lines.append(line)
        self.summary_path.write_text('\n'.join(self.summary_lines) + '\n', encoding='utf-8')

    def fail(self, stage: str, message: str) -> None:
        if self.first_failure is None:
            self.first_failure = stage
        self.note(f'FAIL {stage}: {message}')
        raise HarnessError(message)


def parse_repo_full_name(url: str) -> str:
    for regex in (SSH_RE, HTTPS_RE):
        match = regex.match(url.strip())
        if match:
            return f'{match.group(1)}/{match.group(2)}'
    raise HarnessError(f'Unsupported GitHub URL format: {url}')


def repo_key(repo_full_name: str) -> str:
    return repo_full_name.replace('/', '_')


def now_utc() -> datetime:
    return datetime.now(timezone.utc)


def now_iso() -> str:
    return now_utc().isoformat(timespec='seconds').replace('+00:00', 'Z')


def run_stamp() -> str:
    return now_utc().strftime('%Y%m%dT%H%M%SZ')


def render_issue_body(fixture_text: str, run_id: str, backend: str, scenario: str) -> str:
    block = textwrap.dedent(
        f'''\

        <!-- githubclaw-product-e2e -->
        run_id: {run_id}
        backend: {backend}
        scenario: {scenario}
        created_at: {now_iso()}
        <!-- /githubclaw-product-e2e -->
        '''
    ).strip()
    return fixture_text.rstrip() + '\n\n' + block + '\n'


def replace_backend_frontmatter(text: str, backend: str) -> str:
    return re.sub(r'(^backend:\s*)(?:claude-code|codex)\s*$', rf'\1{backend}', text, count=1, flags=re.MULTILINE)


def normalize_detail_shape(detail: str) -> str:
    text = NUMBER_RE.sub('<n>', detail)
    text = WHITESPACE_RE.sub(' ', text).strip()
    return text


def summarize_loop_patterns(timeline: list[dict[str, Any]]) -> dict[str, Any]:
    counts: dict[str, int] = {}
    for item in timeline:
        pattern = '|'.join(
            [
                str(item.get('agent_type', 'unknown')),
                str(item.get('status', 'unknown')),
                normalize_detail_shape(str(item.get('detail', ''))),
            ]
        )
        counts[pattern] = counts.get(pattern, 0) + 1
    patterns = [
        {'pattern': pattern, 'count': count}
        for pattern, count in sorted(counts.items(), key=lambda item: (-item[1], item[0]))
    ]
    return {'patterns': patterns, 'max_pattern_count': max(counts.values(), default=0)}


def ensure_directory(path: pathlib.Path) -> pathlib.Path:
    path.mkdir(parents=True, exist_ok=True)
    return path


def env_without_githubclaw_home() -> dict[str, str]:
    env = os.environ.copy()
    env.pop('GITHUBCLAW_HOME', None)
    return env


def command_exists(name: str) -> bool:
    return shutil.which(name) is not None


def shared_server_running(env: dict[str, str]) -> bool:
    result = subprocess.run(['githubclaw', 'status'], text=True, capture_output=True, env=env)
    combined = (result.stdout or '') + (result.stderr or '')
    return 'Webhook server is running' in combined


class ServerLease:
    def __init__(self, run: 'HarnessRun') -> None:
        self.run = run
        self.shared_was_running = False
        self.process: subprocess.Popen[str] | None = None
        self.log_handle: Any = None

    def start(self) -> None:
        self.shared_was_running = shared_server_running(self.run.shared_env)
        if self.shared_was_running:
            self.run.artifacts.note('Stopping shared GithubClaw server for exclusive E2E window.')
            self.run.run_cmd(['githubclaw', 'stop'], env=self.run.shared_env, check=False)
            time.sleep(2)
        ensure_directory(self.run.config.server_log_path.parent)
        self.log_handle = self.run.config.server_log_path.open('a', encoding='utf-8')
        self.run.artifacts.note(
            f'Starting isolated GithubClaw server with GITHUBCLAW_HOME={self.run.config.githubclaw_home} on port {self.run.config.port}.'
        )
        self.process = subprocess.Popen(
            ['githubclaw', 'serve', '--port', str(self.run.config.port)],
            cwd=ROOT,
            text=True,
            env=self.run.run_env,
            stdout=self.log_handle,
            stderr=subprocess.STDOUT,
        )
        deadline = time.time() + 30
        while time.time() < deadline:
            if self.process.poll() is not None:
                raise HarnessError('Isolated GithubClaw server exited during startup.')
            if self.run.health_check():
                self.run.artifacts.note(f'Isolated server healthy at {self.run.config.health_url}.')
                return
            time.sleep(2)
        raise HarnessError(f'Isolated GithubClaw server did not become healthy at {self.run.config.health_url}.')

    def stop(self) -> None:
        if self.process is not None and self.process.poll() is None:
            self.run.artifacts.note('Stopping isolated GithubClaw server.')
            self.process.terminate()
            try:
                self.process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        if self.log_handle is not None:
            self.log_handle.close()
        if self.shared_was_running:
            self.run.artifacts.note('Restarting shared GithubClaw server after E2E window.')
            result = subprocess.run(
                ['githubclaw', 'start'],
                text=True,
                capture_output=True,
                env=self.run.shared_env,
            )
            self.run.append_command_log(result.stdout or '')
            self.run.append_command_log(result.stderr or '')
            self.run.append_command_log(f'[shared restart exit={result.returncode}]')
            if result.returncode != 0:
                raise HarnessError('Failed to restart shared GithubClaw server after E2E run.')


class HarnessRun:
    def __init__(self, config: HarnessConfig, scenario: str, backend: str):
        self.config = config
        self.scenario = scenario
        self.backend = backend
        self.run_env = os.environ.copy()
        self.run_env['GITHUBCLAW_HOME'] = str(config.githubclaw_home)
        self.shared_env = env_without_githubclaw_home()
        run_id = f'{scenario}-{backend}-{run_stamp()}-{random.randint(1000, 9999)}'
        root = ensure_directory(config.product_e2e_dir / run_id)
        raw = ensure_directory(root / 'raw')
        self.artifacts = RunArtifacts(
            run_id=run_id,
            scenario=scenario,
            backend=backend,
            run_dir=root,
            raw_dir=raw,
            commands_log=root / 'raw' / 'commands.log',
            summary_path=root / 'summary.md',
            verdict_path=root / 'verdict.json',
        )
        self.log_offset = self.config.server_log_path.stat().st_size if self.config.server_log_path.exists() else 0
        self.started_at_unix = time.time()
        self.baseline_issue_comments = 0
        self.issue_number: int | None = None
        self.issue_url: str | None = None
        self.issue_title: str | None = None
        self.server_lease = ServerLease(self)
        self.write_json(
            self.artifacts.verdict_path,
            {
                'pass': False,
                'scenario': scenario,
                'backend': backend,
                'run_id': run_id,
                'first_failing_stage': None,
            },
        )
        self.artifacts.note(f'Started {scenario} run `{run_id}` for backend `{backend}`.')

    def append_command_log(self, entry: str) -> None:
        if not entry:
            return
        with self.artifacts.commands_log.open('a', encoding='utf-8') as handle:
            handle.write(entry)
            if not entry.endswith('\n'):
                handle.write('\n')

    def run_cmd(
        self,
        cmd: list[str],
        *,
        cwd: pathlib.Path | None = None,
        env: dict[str, str] | None = None,
        check: bool = True,
    ) -> subprocess.CompletedProcess[str]:
        cwd = cwd or ROOT
        proc = subprocess.run(cmd, cwd=cwd, text=True, capture_output=True, env=env or self.run_env)
        self.append_command_log(f'$ (cwd={cwd}) {' '.join(cmd)}')
        self.append_command_log(proc.stdout or '')
        self.append_command_log(proc.stderr or '')
        self.append_command_log(f'[exit={proc.returncode}]')
        if check and proc.returncode != 0:
            raise HarnessError(f'Command failed ({proc.returncode}): {' '.join(cmd)}')
        return proc

    def gh_json(self, args: list[str]) -> Any:
        proc = self.run_cmd(['gh', *args], cwd=self.config.checkout_dir)
        return json.loads(proc.stdout)

    def write_json(self, path: pathlib.Path, data: Any) -> None:
        ensure_directory(path.parent)
        path.write_text(json.dumps(data, indent=2, sort_keys=True) + '\n', encoding='utf-8')

    def update_verdict(self, passed: bool) -> None:
        self.write_json(
            self.artifacts.verdict_path,
            {
                'pass': passed,
                'scenario': self.scenario,
                'backend': self.backend,
                'run_id': self.artifacts.run_id,
                'issue_number': self.issue_number,
                'issue_url': self.issue_url,
                'first_failing_stage': self.artifacts.first_failure,
                'githubclaw_home': str(self.config.githubclaw_home),
            },
        )

    def preflight(self) -> None:
        for command in ('gh', 'git', 'curl', 'make', 'python3', 'githubclaw'):
            if not command_exists(command):
                self.artifacts.fail('preflight', f'Required command `{command}` not found in PATH.')
        self.run_cmd(['gh', 'auth', 'status'], env=self.shared_env)
        self.ensure_checkout()
        self.ensure_repo_initialized()
        self.copy_shared_webhook_secret()
        self.ensure_backend_overrides()
        self.server_lease.start()
        self.log_offset = self.config.server_log_path.stat().st_size if self.config.server_log_path.exists() else 0
        self.write_json(
            self.artifacts.run_dir / 'preflight.json',
            {
                'repo': self.config.repo_full_name,
                'checkout_dir': str(self.config.checkout_dir),
                'githubclaw_home': str(self.config.githubclaw_home),
                'shared_githubclaw_home': str(self.config.shared_githubclaw_home),
                'server_log_path': str(self.config.server_log_path),
                'health_url': self.config.health_url,
                'backend': self.backend,
            },
        )

    def ensure_checkout(self) -> None:
        ensure_directory(self.config.checkout_dir.parent)
        if self.config.checkout_dir.exists():
            shutil.rmtree(self.config.checkout_dir)
        self.artifacts.note(f'Cloning sandbox repo to {self.config.checkout_dir}.')
        self.run_cmd(['git', 'clone', self.config.sandbox_url, str(self.config.checkout_dir)], env=self.shared_env)

    def ensure_repo_initialized(self) -> None:
        self.artifacts.note(f'Initializing isolated GithubClaw home at {self.config.githubclaw_home}.')
        self.run_cmd(['githubclaw', 'init'], cwd=self.config.checkout_dir)

    def copy_shared_webhook_secret(self) -> None:
        ensure_directory(self.config.webhook_secret_path.parent)
        if self.config.shared_webhook_secret_path.exists():
            shutil.copy2(self.config.shared_webhook_secret_path, self.config.webhook_secret_path)
            self.artifacts.note('Copied shared webhook secret into isolated GithubClaw home.')
            return
        self.artifacts.fail(
            'preflight',
            f'Shared webhook secret not found at {self.config.shared_webhook_secret_path}.',
        )

    def ensure_backend_overrides(self) -> None:
        defaults_dir = ROOT / 'defaults' / 'agents'
        ensure_directory(self.config.repo_override_agents_dir)
        for source in defaults_dir.glob('*.md'):
            target = self.config.repo_override_agents_dir / source.name
            target.write_text(
                replace_backend_frontmatter(source.read_text(encoding='utf-8'), self.backend),
                encoding='utf-8',
            )
        self.artifacts.note(f'Installed isolated agent overrides for backend `{self.backend}`.')

    def health_check(self) -> bool:
        try:
            with urllib.request.urlopen(self.config.health_url, timeout=5) as response:
                payload = json.loads(response.read().decode('utf-8'))
            self.write_json(self.artifacts.run_dir / 'health.json', {'url': self.config.health_url, 'response': payload})
            return payload.get('status') == 'ok'
        except Exception:
            return False

    def fixture_text(self) -> str:
        return self.config.fixture_path.read_text(encoding='utf-8')

    def create_issue(self) -> None:
        title = f'[product-e2e][{self.backend}] sandbox bug fixture {self.artifacts.run_id}'
        body = render_issue_body(self.fixture_text(), self.artifacts.run_id, self.backend, self.scenario)
        issue_body_path = self.artifacts.raw_dir / 'issue-body.md'
        issue_body_path.write_text(body, encoding='utf-8')
        proc = self.run_cmd(
            [
                'gh', 'issue', 'create',
                '--repo', self.config.repo_full_name,
                '--title', title,
                '--body-file', str(issue_body_path),
            ],
            cwd=self.config.checkout_dir,
            env=self.shared_env,
        )
        self.issue_url = proc.stdout.strip().splitlines()[-1]
        issue_json = json.loads(
            self.run_cmd(
                [
                    'gh', 'issue', 'view', self.issue_url,
                    '--repo', self.config.repo_full_name,
                    '--json', 'number,url,title,body,comments,state,author,createdAt',
                ],
                cwd=self.config.checkout_dir,
                env=self.shared_env,
            ).stdout
        )
        self.issue_number = int(issue_json['number'])
        self.issue_title = issue_json['title']
        self.baseline_issue_comments = len(issue_json.get('comments', []))
        self.write_json(self.artifacts.run_dir / 'issue.json', issue_json)
        self.artifacts.note(f'Created sandbox issue #{self.issue_number}: {self.issue_url}')

    def read_new_log_excerpt(self) -> str:
        if not self.config.server_log_path.exists():
            return ''
        with self.config.server_log_path.open('r', encoding='utf-8', errors='replace') as handle:
            handle.seek(self.log_offset)
            return handle.read()

    def wait_for_webhook_and_queue(self) -> None:
        assert self.issue_number is not None
        deadline = time.time() + self.config.timeout_seconds
        event_label = 'issues_opened'
        while time.time() < deadline:
            excerpt = self.read_new_log_excerpt()
            queue_files = sorted(str(path) for path in self.config.queue_dir.rglob('*.json')) if self.config.queue_dir.exists() else []
            if f'Queued event {event_label} for {self.config.repo_full_name}' in excerpt or queue_files:
                self.write_json(
                    self.artifacts.run_dir / 'webhook.json',
                    {'event_label': event_label, 'repo': self.config.repo_full_name, 'log_excerpt': excerpt[-4000:]},
                )
                self.write_json(
                    self.artifacts.run_dir / 'queue.json',
                    {'queue_dir': str(self.config.queue_dir), 'files': queue_files},
                )
                self.artifacts.note('Observed webhook ingress / queue evidence.')
                return
            time.sleep(2)
        self.artifacts.fail('webhook', 'Timed out waiting for webhook/queue evidence.')

    def runtime_snapshot_path(self) -> pathlib.Path:
        assert self.issue_number is not None
        return self.config.sessions_repo_dir / str(self.issue_number) / 'runtime.json'

    def wait_for_routing_and_session(self) -> None:
        deadline = time.time() + self.config.timeout_seconds
        path = self.runtime_snapshot_path()
        while time.time() < deadline:
            if path.exists():
                snapshot = json.loads(path.read_text(encoding='utf-8'))
                self.write_json(
                    self.artifacts.run_dir / 'routing.json',
                    {'root_issue': self.issue_number, 'title': snapshot.get('title'), 'repo': snapshot.get('repo')},
                )
                self.write_json(
                    self.artifacts.run_dir / 'session.json',
                    {'session_dir': str(path.parent), 'runtime_snapshot_path': str(path), 'runtime_snapshot': snapshot},
                )
                self.artifacts.note(f'Observed runtime snapshot for issue #{self.issue_number}.')
                return
            time.sleep(2)
        self.artifacts.fail('routing', f'Timed out waiting for runtime snapshot at {path}.')

    def collect_outcome(self) -> dict[str, Any]:
        receipts: list[dict[str, Any]] = []
        if self.config.dispatch_receipts_dir.exists():
            for path in self.config.dispatch_receipts_dir.glob('*.json'):
                if path.stat().st_mtime >= self.started_at_unix - 1:
                    try:
                        payload = json.loads(path.read_text(encoding='utf-8'))
                    except json.JSONDecodeError:
                        continue
                    payload['path'] = str(path)
                    receipts.append(payload)
        issue = json.loads(
            self.run_cmd(
                ['gh', 'issue', 'view', str(self.issue_number), '--repo', self.config.repo_full_name, '--json', 'number,url,comments,state,title'],
                cwd=self.config.checkout_dir,
                env=self.shared_env,
            ).stdout
        )
        prs = json.loads(
            self.run_cmd(
                ['gh', 'pr', 'list', '--repo', self.config.repo_full_name, '--state', 'all', '--search', f'ref #{self.issue_number} in:body', '--json', 'number,title,url,state,createdAt,body'],
                cwd=self.config.checkout_dir,
                env=self.shared_env,
            ).stdout
        )
        comments = issue.get('comments', [])
        new_comments = [
            {
                'url': comment.get('url'),
                'author': (comment.get('author') or {}).get('login'),
                'body_excerpt': (comment.get('body') or '')[:500],
                'createdAt': comment.get('createdAt'),
            }
            for comment in comments[self.baseline_issue_comments:]
        ]
        return {'dispatch_receipts': receipts, 'issue': issue, 'new_comments': new_comments, 'pull_requests': prs}

    def wait_for_observable_outcome(self) -> dict[str, Any]:
        deadline = time.time() + self.config.timeout_seconds
        while time.time() < deadline:
            outcome = self.collect_outcome()
            if outcome['dispatch_receipts'] or outcome['new_comments'] or outcome['pull_requests']:
                self.write_json(
                    self.artifacts.run_dir / 'dispatch.json',
                    {
                        'dispatch_receipt_keys': [item['key'] for item in outcome['dispatch_receipts']],
                        'dispatch_receipts': outcome['dispatch_receipts'],
                    },
                )
                self.write_json(
                    self.artifacts.run_dir / 'github.json',
                    {'issue_url': self.issue_url, 'new_comments': outcome['new_comments'], 'pull_requests': outcome['pull_requests']},
                )
                self.artifacts.note('Observed post-issue outcome on GitHub or via dispatch receipt.')
                return outcome
            time.sleep(3)
        self.artifacts.fail('outcome', 'Timed out waiting for dispatch/comment/PR evidence.')

    def receipts_for_event_id(self, event_id: str) -> list[dict[str, Any]]:
        receipts: list[dict[str, Any]] = []
        if not self.config.dispatch_receipts_dir.exists():
            return receipts
        for path in self.config.dispatch_receipts_dir.glob('*.json'):
            try:
                payload = json.loads(path.read_text(encoding='utf-8'))
            except json.JSONDecodeError:
                continue
            if payload.get('event_id') == event_id:
                payload['path'] = str(path)
                receipts.append(payload)
        receipts.sort(key=lambda item: item['key'])
        return receipts

    def post_signed_webhook(self, payload: dict[str, Any], event_type: str, delivery_id: str, secret: str) -> None:
        body = json.dumps(payload).encode('utf-8')
        signature = 'sha256=' + hmac.new(secret.encode('utf-8'), body, hashlib.sha256).hexdigest()
        request = urllib.request.Request(
            self.config.health_url.replace('/health', '/webhook'),
            data=body,
            method='POST',
            headers={
                'Content-Type': 'application/json',
                'X-GitHub-Event': event_type,
                'X-GitHub-Delivery': delivery_id,
                'X-Hub-Signature-256': signature,
            },
        )
        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                response.read()
        except urllib.error.HTTPError as exc:
            raise HarnessError(f'Webhook replay failed with HTTP {exc.code}: {exc.read().decode("utf-8", "replace")}') from exc

    def wait_for_receipt_stability(self, event_id: str) -> list[dict[str, Any]]:
        deadline = time.time() + min(self.config.timeout_seconds, 60)
        last: list[str] | None = None
        while time.time() < deadline:
            current = self.receipts_for_event_id(event_id)
            keys = [item['key'] for item in current]
            if keys == last:
                return current
            last = keys
            time.sleep(2)
        return self.receipts_for_event_id(event_id)

    def perform_replay_idempotency(self) -> None:
        assert self.issue_number is not None
        secret = self.config.webhook_secret_path.read_text(encoding='utf-8').strip()
        issue_data = json.loads((self.artifacts.run_dir / 'issue.json').read_text(encoding='utf-8'))
        delivery_id = f'{self.artifacts.run_id}-replay'
        payload = {
            'action': 'opened',
            'repository': {'full_name': self.config.repo_full_name},
            'issue': {'number': self.issue_number, 'title': issue_data['title'], 'body': issue_data['body']},
        }
        before = self.receipts_for_event_id(delivery_id)
        self.post_signed_webhook(payload, 'issues', delivery_id, secret)
        time.sleep(3)
        first = self.wait_for_receipt_stability(delivery_id)
        self.post_signed_webhook(payload, 'issues', delivery_id, secret)
        time.sleep(3)
        second = self.wait_for_receipt_stability(delivery_id)
        result = {
            'delivery_id': delivery_id,
            'before_count': len(before),
            'after_first_count': len(first),
            'after_second_count': len(second),
            'receipt_keys_after_first': [item['key'] for item in first],
            'receipt_keys_after_second': [item['key'] for item in second],
            'passed': len(first) == len(second),
        }
        self.write_json(self.artifacts.run_dir / 'replay.json', result)
        if len(first) != len(second):
            self.artifacts.fail('replay', 'Replay increased successful dispatch receipt count.')
        self.artifacts.note('Replay/idempotency check passed.')

    def perform_restart_resume(self) -> None:
        path = self.runtime_snapshot_path()
        previous = json.loads(path.read_text(encoding='utf-8'))
        previous_updated = previous.get('updated_at_unix_seconds', 0)
        previous_timeline = len(previous.get('agent_timeline', []))
        self.artifacts.note('Restarting isolated server for resume probe.')
        self.server_lease.stop()
        time.sleep(2)
        self.server_lease = ServerLease(self)
        self.server_lease.start()
        self.run_cmd(
            ['gh', 'issue', 'comment', str(self.issue_number), '--repo', self.config.repo_full_name, '--body', f'Product E2E resume probe for {self.artifacts.run_id}'],
            cwd=self.config.checkout_dir,
            env=self.shared_env,
        )
        deadline = time.time() + self.config.timeout_seconds
        while time.time() < deadline:
            if path.exists():
                snapshot = json.loads(path.read_text(encoding='utf-8'))
                if snapshot.get('updated_at_unix_seconds', 0) > previous_updated or len(snapshot.get('agent_timeline', [])) > previous_timeline:
                    self.write_json(
                        self.artifacts.run_dir / 'resume.json',
                        {
                            'passed': True,
                            'session_dir': str(path.parent),
                            'previous_updated_at': previous_updated,
                            'current_updated_at': snapshot.get('updated_at_unix_seconds'),
                            'previous_timeline_count': previous_timeline,
                            'current_timeline_count': len(snapshot.get('agent_timeline', [])),
                        },
                    )
                    self.artifacts.note('Restart/resume probe passed.')
                    return
            time.sleep(3)
        self.artifacts.fail('resume', 'Timed out waiting for runtime snapshot update after restart probe.')

    def perform_loop_guard(self) -> None:
        path = self.runtime_snapshot_path()
        self.artifacts.note(f'Observing loop patterns for {self.config.loop_window_seconds}s.')
        deadline = time.time() + self.config.loop_window_seconds
        latest: dict[str, Any] = {'agent_timeline': []}
        while time.time() < deadline:
            if path.exists():
                try:
                    latest = json.loads(path.read_text(encoding='utf-8'))
                except json.JSONDecodeError:
                    pass
            time.sleep(5)
        summary = summarize_loop_patterns(latest.get('agent_timeline', []))
        summary.update(
            {
                'threshold': self.config.loop_threshold,
                'passed': summary['max_pattern_count'] <= self.config.loop_threshold,
                'observation_window_seconds': self.config.loop_window_seconds,
            }
        )
        self.write_json(self.artifacts.run_dir / 'loop_guard.json', summary)
        if not summary['passed']:
            self.artifacts.fail('loop_guard', f"Loop threshold exceeded (max pattern count {summary['max_pattern_count']}).")
        self.artifacts.note('Loop guard observation passed.')

    def finalize(self, passed: bool) -> None:
        self.update_verdict(passed)
        if passed:
            self.artifacts.note('Run completed successfully.')

    def execute(self, live: bool) -> None:
        try:
            self.preflight()
            self.create_issue()
            self.wait_for_webhook_and_queue()
            self.wait_for_routing_and_session()
            self.wait_for_observable_outcome()
            if live:
                self.perform_replay_idempotency()
                self.perform_restart_resume()
                self.perform_loop_guard()
            self.finalize(True)
        except HarnessError:
            self.update_verdict(False)
            raise
        finally:
            self.server_lease.stop()

    def run_smoke(self) -> None:
        self.execute(live=False)

    def run_live(self) -> None:
        self.execute(live=True)


def build_config(args: argparse.Namespace) -> HarnessConfig:
    sandbox_url = args.sandbox_url or DEFAULT_SANDBOX_URL
    repo_full_name = parse_repo_full_name(sandbox_url)
    repo_name = repo_full_name.split('/')[-1]
    fixture_path = pathlib.Path(args.fixture or DEFAULT_FIXTURE).resolve()
    isolated_home = pathlib.Path(args.isolated_home).expanduser().resolve()
    run_root = isolated_home.parent
    shared_home = pathlib.Path(
        os.environ.get('PRODUCT_E2E_SHARED_GITHUBCLAW_HOME', pathlib.Path.home() / '.githubclaw')
    ).expanduser()
    checkout_dir = pathlib.Path(args.checkout_dir).expanduser().resolve() if args.checkout_dir else (run_root / 'checkout' / repo_name)
    return HarnessConfig(
        sandbox_url=sandbox_url,
        repo_full_name=repo_full_name,
        repo_key=repo_key(repo_full_name),
        githubclaw_home=isolated_home,
        shared_githubclaw_home=shared_home,
        checkout_dir=checkout_dir,
        fixture_path=fixture_path,
        timeout_seconds=int(args.timeout or DEFAULT_TIMEOUT_SECONDS),
        loop_window_seconds=int(args.loop_window or DEFAULT_LOOP_WINDOW_SECONDS),
        loop_threshold=int(args.loop_threshold or DEFAULT_LOOP_THRESHOLD),
        port=int(args.port or DEFAULT_HEALTH_PORT),
        auto_restart=DEFAULT_AUTO_RESTART if args.auto_restart is None else args.auto_restart,
        run_root=run_root,
    )


def latest_run_dir(home: pathlib.Path, repo_name: str) -> pathlib.Path | None:
    repo_dir = home / 'runtime' / repo_key(repo_name) / 'product-e2e'
    if not repo_dir.exists():
        return None
    runs = [path for path in repo_dir.iterdir() if path.is_dir()]
    if not runs:
        return None
    return sorted(runs, key=lambda path: path.stat().st_mtime)[-1]


def release_run(args: argparse.Namespace, repo_full_name: str) -> int:
    release_id = f'release-gate-{run_stamp()}-{random.randint(1000, 9999)}'
    release_root = ensure_directory(DEFAULT_E2E_ROOT / release_id)
    summary_lines = [f'# Product E2E Release Gate `{release_id}`', '']
    matrix: list[dict[str, Any]] = []

    smoke_backend = args.smoke_backend or 'claude-code'
    lanes = [('smoke', smoke_backend), ('live', 'claude-code'), ('live', 'codex')]
    script_path = pathlib.Path(__file__).resolve()

    for lane, backend in lanes:
        lane_root = ensure_directory(release_root / f'{lane}-{backend}')
        isolated_home = lane_root / 'home'
        checkout_dir = lane_root / 'checkout'
        log_path = lane_root / 'runner.log'
        cmd = [
            sys.executable,
            str(script_path),
            lane,
            '--backend',
            backend,
            '--isolated-home',
            str(isolated_home),
            '--checkout-dir',
            str(checkout_dir),
            '--sandbox-url',
            args.sandbox_url or DEFAULT_SANDBOX_URL,
        ]
        if args.fixture:
            cmd.extend(['--fixture', args.fixture])
        if args.timeout:
            cmd.extend(['--timeout', str(args.timeout)])
        if args.port:
            cmd.extend(['--port', str(args.port)])
        if args.loop_window:
            cmd.extend(['--loop-window', str(args.loop_window)])
        if args.loop_threshold:
            cmd.extend(['--loop-threshold', str(args.loop_threshold)])
        proc = subprocess.run(cmd, text=True, capture_output=True)
        log_path.write_text((proc.stdout or '') + '\n' + (proc.stderr or ''), encoding='utf-8')
        run_dir = latest_run_dir(isolated_home, repo_full_name)
        passed = proc.returncode == 0
        matrix.append({'lane': lane, 'backend': backend, 'pass': passed, 'run_dir': str(run_dir) if run_dir else None, 'isolated_home': str(isolated_home), 'log': str(log_path)})
        summary_lines.append(f'- {lane} ({backend}): {'PASS' if passed else 'FAIL'}')

    overall = all(item['pass'] for item in matrix)
    (release_root / 'summary.md').write_text('\n'.join(summary_lines) + '\n', encoding='utf-8')
    (release_root / 'matrix-summary.json').write_text(json.dumps({'release_run_id': release_id, 'overall_pass': overall, 'matrix': matrix}, indent=2) + '\n', encoding='utf-8')
    return 0 if overall else 1


def add_common_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument('--sandbox-url', default=None)
    parser.add_argument('--checkout-dir', default=None)
    parser.add_argument('--fixture', default=None)
    parser.add_argument('--timeout', default=None)
    parser.add_argument('--port', default=None)
    parser.add_argument('--loop-window', default=None)
    parser.add_argument('--loop-threshold', default=None)
    parser.add_argument('--auto-restart', action=argparse.BooleanOptionalAction, default=None)
    parser.add_argument('--isolated-home', default=None)


def default_isolated_home(command: str, backend: str) -> pathlib.Path:
    return ensure_directory(DEFAULT_E2E_ROOT / f'{command}-{backend}-{run_stamp()}-{random.randint(1000, 9999)}') / 'home'


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description='Maintainer-only GithubClaw product E2E harness.')
    subparsers = parser.add_subparsers(dest='command', required=True)

    smoke = subparsers.add_parser('smoke')
    add_common_arguments(smoke)
    smoke.add_argument('--backend', default=os.environ.get('PRODUCT_E2E_SMOKE_BACKEND', 'claude-code'))

    live = subparsers.add_parser('live')
    add_common_arguments(live)
    live.add_argument('--backend', choices=['claude-code', 'codex'], required=True)

    release = subparsers.add_parser('release')
    add_common_arguments(release)
    release.add_argument('--smoke-backend', default=os.environ.get('PRODUCT_E2E_SMOKE_BACKEND', 'claude-code'))

    args = parser.parse_args(argv)
    if args.command in {'smoke', 'live'} and not args.isolated_home:
        args.isolated_home = str(default_isolated_home(args.command, args.backend))

    try:
        if args.command == 'release':
            repo_full_name = parse_repo_full_name(args.sandbox_url or DEFAULT_SANDBOX_URL)
            return release_run(args, repo_full_name)

        config = build_config(args)
        run = HarnessRun(config, args.command, args.backend)
        if args.command == 'smoke':
            run.run_smoke()
        else:
            run.run_live()
        print(f'product-e2e run artifacts: {run.artifacts.run_dir}')
        return 0
    except HarnessError as exc:
        print(f'product-e2e: {exc}', file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())
