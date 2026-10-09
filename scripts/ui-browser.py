#!/usr/bin/env python3
"""Run the locked browser acceptance suite with all generated files outside Git."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--install-deps', action='store_true',
                        help='Install Chromium OS dependencies (CI; may require sudo)')
    parser.add_argument('--grep', help='Select a named causal control; normal CI runs every case')
    args = parser.parse_args()
    repo = Path(__file__).resolve().parent.parent
    binary = os.environ.get('PBPS_TEST_UI_BIN', '')
    if not Path(binary).is_absolute() or not Path(binary).is_file():
        parser.error('PBPS_TEST_UI_BIN must name the absolute freshly built pbps binary')
    if not os.environ.get('PBPS_TEST_UI_PG_URL'):
        parser.error('PBPS_TEST_UI_PG_URL must name the disposable PostgreSQL fixture server')
    common = Path(subprocess.check_output(
        ['git', '-C', str(repo), 'rev-parse', '--path-format=absolute', '--git-common-dir'],
        text=True).strip()).parent
    temp_base = Path(tempfile.gettempdir()).resolve()
    if temp_base.is_relative_to(repo) or temp_base.is_relative_to(common):
        parser.error('TMPDIR must be outside the repository and its worktrees')
    # A unique staging tree also makes module resolution independent of the caller.
    with tempfile.TemporaryDirectory(prefix='pbps-ui-browser-') as temporary:
        stage = Path(temporary).resolve()
        if stage.is_relative_to(repo) or stage.is_relative_to(common):
            parser.error('Temporary storage must be outside the repository and its worktrees')
        for source in (repo / 'tests/ui-browser').iterdir():
            if source.is_file():
                shutil.copyfile(source, stage / source.name)
        env = os.environ.copy()
        env['PBPS_BROWSER_TMP'] = str(stage)
        env.pop('NO_COLOR', None)
        env['FORCE_COLOR'] = '0'
        env['npm_config_cache'] = str(stage / 'npm-cache')
        # Reuse only the external browser download cache; do not create a repo cache.
        cache = Path(env.get('PLAYWRIGHT_BROWSERS_PATH',
                             str(Path(tempfile.gettempdir()) / 'pbps-browser-cache'))).resolve()
        if cache.is_relative_to(repo) or cache.is_relative_to(common):
            parser.error('PLAYWRIGHT_BROWSERS_PATH must be outside the repository')
        env['PLAYWRIGHT_BROWSERS_PATH'] = str(cache)
        def run(command):
            subprocess.run(command, cwd=stage, env=env, check=True)
        run(['node', '--version'])
        run(['npm', 'ci', '--ignore-scripts', '--no-audit', '--no-fund'])
        playwright = str(stage / 'node_modules/.bin/playwright')
        run([playwright, 'install', *(['--with-deps'] if args.install_deps else []), 'chromium'])
        run([playwright, 'test', '--config', str(stage / 'playwright.config.mjs'),
             *(['--grep', args.grep] if args.grep else [])])


if __name__ == '__main__':
    main()
