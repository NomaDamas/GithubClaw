import importlib.util
import pathlib
import sys
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / 'scripts' / 'product_e2e.py'
SPEC = importlib.util.spec_from_file_location('product_e2e', SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


class ProductE2eHelpersTest(unittest.TestCase):
    def test_parse_repo_full_name_supports_ssh_and_https(self):
        self.assertEqual(
            MODULE.parse_repo_full_name('git@github.com:vkehfdl1/GithubClaw-Sandbox.git'),
            'vkehfdl1/GithubClaw-Sandbox',
        )
        self.assertEqual(
            MODULE.parse_repo_full_name('https://github.com/vkehfdl1/GithubClaw-Sandbox.git'),
            'vkehfdl1/GithubClaw-Sandbox',
        )

    def test_render_issue_body_adds_metadata_block(self):
        rendered = MODULE.render_issue_body('body text', 'run-123', 'codex', 'smoke')
        self.assertIn('body text', rendered)
        self.assertIn('run_id: run-123', rendered)
        self.assertIn('backend: codex', rendered)
        self.assertIn('scenario: smoke', rendered)

    def test_replace_backend_updates_frontmatter_only(self):
        original = '---\nbackend: claude-code\n---\n\n# Agent\n'
        updated = MODULE.replace_backend_frontmatter(original, 'codex')
        self.assertIn('backend: codex', updated)
        self.assertNotIn('backend: claude-code', updated)

    def test_summarize_loop_patterns_groups_similar_details(self):
        timeline = [
            {'agent_type': 'orchestrator', 'status': 'running', 'detail': 'Dispatch started for repo#12'},
            {'agent_type': 'orchestrator', 'status': 'running', 'detail': 'Dispatch started for repo#12'},
            {'agent_type': 'orchestrator', 'status': 'running', 'detail': 'Dispatch started for repo#13'},
            {'agent_type': 'verifier', 'status': 'completed', 'detail': 'Dispatch completed for repo#12'},
        ]
        summary = MODULE.summarize_loop_patterns(timeline)
        repeated = {item['pattern']: item['count'] for item in summary['patterns']}
        self.assertEqual(repeated['orchestrator|running|Dispatch started for repo#<n>'], 3)
        self.assertEqual(repeated['verifier|completed|Dispatch completed for repo#<n>'], 1)
        self.assertEqual(summary['max_pattern_count'], 3)


if __name__ == '__main__':
    unittest.main()
