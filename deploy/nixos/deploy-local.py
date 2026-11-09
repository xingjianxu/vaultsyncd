#!/usr/bin/env python3
"""固定 Git 提交并纯构建本机 NixOS：build → dry-activate → test → 健康 → switch。

默认使用当前项目 HEAD 和 /etc/nixos。只更新 vaultsync.rev，不更新依赖或清空数据。
失败保留证据、工作树和旧 system 的 GC root；激活后失败尝试恢复旧系统。
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request

PURE = ['--option', 'pure-eval', 'true', '--no-update-lock-file', '--no-write-lock-file']


def command(args, *, capture=True, **kwargs):
    args = list(map(str, args))
    print('+ ' + shlex.join(args), flush=True)
    return subprocess.run(args, check=True, text=True,
                          stdout=subprocess.PIPE if capture else None, **kwargs).stdout


def git(repo, *args):
    return command(['git', '-C', repo, *args]).strip()


def clean(repo):
    if git(repo, 'status', '--porcelain'):
        raise RuntimeError(f'{repo} 有未提交修改，请先提交；部署不会 stash 或搬运修改')


def pin(text, revision):
    # 仅接受当前主机的字面配置块；遇到重复/动态表达式拒绝猜测。
    pattern = r'(\bvaultsync\s*=\s*\{\s*enable\s*=\s*true\s*;\s*rev\s*=\s*")[0-9a-f]{40}("\s*;)'
    result, count = re.subn(pattern, lambda m: m[1] + revision + m[2], text)
    if count != 1:
        raise ValueError('需要唯一 vaultsync = { enable = true; rev = "完整SHA"; ... } 字面配置')
    return result


def configuration(repo, host):
    expression = '''c: {
      enabled = c.features.server.vaultsync.enable;
      repository = c.features.server.vaultsync.repository;
      inherit (c.services.vaultsyncd) user group stateDir;
      vaultPaths = c.services.vaultsyncd.vaultPaths or c.services.vaultsyncd.rootPaths or [ ];
      package = toString c.services.vaultsyncd.package;
      inherit (c.features.server.vaultsync) port;
      domain = if c.features.server.vaultsync.caddy.enable
        then c.features.server.vaultsync.caddy.domain else null;
    }'''
    return json.loads(command(['nix', 'eval', *PURE, '--json',
                              f'{repo}#nixosConfigurations.{host}.config', '--apply', expression]))


def health(cfg, *, previous_id=None, verify_runtime=True):
    origin = f'https://{cfg["domain"]}' if cfg['domain'] else f'http://127.0.0.1:{cfg["port"]}'
    def get(path):
        with urllib.request.urlopen(origin + path, timeout=5) as response:
            return json.load(response)
    last = None
    for _ in range(30):
        try:
            if get('/v1/health')['status'] != 'ok':
                raise RuntimeError('服务健康状态不是 ok')
            caps = get('/v1/capabilities')
            if previous_id is not None and caps['server_id'] != previous_id:
                raise RuntimeError('ServerId 发生变化，拒绝完成部署')
            command(['systemctl', 'is-active', '--quiet', 'vaultsyncd.service'])
            if not verify_runtime:
                return caps
            unit = command(['systemctl', 'show', 'vaultsyncd.service', '--property=User,Group,ExecStart'])
            if f'User={cfg["user"]}\n' not in unit or f'Group={cfg["group"]}\n' not in unit or cfg['package'] not in unit:
                raise RuntimeError('运行单元用户/组/发行包与构建结果不一致')
            pid = int(command(['systemctl', 'show', 'vaultsyncd.service', '--property=MainPID', '--value']))
            import pwd
            import grp
            if Path(f'/proc/{pid}').stat().st_uid != pwd.getpwnam(cfg['user']).pw_uid:
                raise RuntimeError('实际进程 UID 不匹配')
            uid, gid = pwd.getpwnam(cfg['user']).pw_uid, grp.getgrnam(cfg['group']).gr_gid
            for path in [cfg['stateDir'], *cfg['vaultPaths']]:
                stat = Path(path).stat()
                if (stat.st_uid, stat.st_gid) != (uid, gid):
                    raise RuntimeError(f'{path} 所有权不匹配，需离线迁移后重试')
            status = json.loads(command(['sudo', '-n', '-u', cfg['user'], Path(cfg['package']) / 'bin/vaultsyncd-admin',
                                        'status', '--socket', Path(cfg['stateDir']) / 'admin/socket', '--json']))
            if status.get('status') != 200 or status.get('result', {}).get('phase') != 'ready':
                raise RuntimeError('IPC 未就绪或需要恢复')
            return caps
        except (urllib.error.URLError, OSError, RuntimeError, KeyError, subprocess.CalledProcessError) as error:
            last = error
            time.sleep(1)
    raise RuntimeError(f'健康检查未通过：{last}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--rev', default='HEAD', help='项目已提交的 commit/tag，默认 HEAD')
    parser.add_argument('--config', type=Path, default=Path('/etc/nixos'))
    parser.add_argument('--host', default=socket.gethostname())
    parser.add_argument('--evidence-base', type=Path, default=Path('/data/vaultsyncd-dev/vaultsyncd-temp/evidence/deploy'))
    parser.add_argument('--build-only', action='store_true', help='只纯构建和保存证据，不激活或修改主机分支')
    args = parser.parse_args()
    if not re.fullmatch(r'[A-Za-z0-9_-]+', args.host):
        parser.error('无效主机名')
    if os.geteuid() == 0:
        parser.error('请以普通用户运行；激活阶段使用 sudo')
    project = Path(__file__).resolve().parents[2]
    config = args.config.resolve(strict=True)
    clean(project)
    clean(config)
    branch = git(config, 'symbolic-ref', '--short', 'HEAD')
    baseline = git(config, 'rev-parse', 'HEAD')
    revision = git(project, 'rev-parse', '--verify', f'{args.rev}^{{commit}}')
    cfg = configuration(config, args.host)
    if not cfg['enabled']:
        raise RuntimeError('主机未启用 vaultsync')
    # 模块从永久仓库读取，不能固定到之后会被清理的任务 worktree。
    repository = cfg['repository']
    if not repository.startswith('git+file:///') or '?' in repository:
        raise RuntimeError('本机脚本要求无 query 的 git+file:/// 永久项目仓库地址')
    permanent = Path(repository.removeprefix('git+file://'))
    if Path(git(permanent, 'rev-parse', '--absolute-git-dir')) != Path(git(permanent, 'rev-parse', '--path-format=absolute', '--git-common-dir')):
        raise RuntimeError('repository 必须指向永久主仓库，不能使用 linked worktree')
    if git(permanent, 'rev-parse', '--verify', f'{revision}^{{commit}}') != revision:
        raise RuntimeError('永久项目仓库中不存在目标提交')
    args.evidence_base.mkdir(parents=True, exist_ok=True)
    with (args.evidence_base / '.deploy.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        deploy(args, project, config, branch, baseline, revision, cfg)


def deploy(args, project, config, branch, baseline, revision, old_cfg):
    run = args.evidence_base / (time.strftime('local-%Y%m%dT%H%M%S-') + str(os.getpid()))
    run.mkdir(mode=0o700)
    tree = run / 'host'
    task_branch = f'chore/S113-deploy-{run.name}'
    old_system = Path('/run/current-system').resolve(strict=True)
    boot_system = Path('/nix/var/nix/profiles/system').resolve(strict=True)
    record = dict(revision=revision, host=args.host, config=str(config), branch=branch,
                  baseline=baseline, previous_system=str(old_system), previous_boot=str(boot_system),
                  worktree=str(tree), deployment_branch=task_branch, passed=False)
    def save():
        (run / 'result.json').write_text(json.dumps(record, indent=2) + '\n')
    save()
    print(f'部署证据：{run}', flush=True)
    # 留存锁、diff、旧 generation 与 GC roots；不复制运行时凭据。
    (run / 'host-before.diff').write_text(git(config, 'diff', 'HEAD'))
    (run / 'host-flake.lock').write_bytes((config / 'flake.lock').read_bytes())
    (run / 'generations.txt').write_text(command(['sudo', '-n', 'nix-env', '-p', '/nix/var/nix/profiles/system', '--list-generations']))
    for name, target in [('previous-system', old_system), ('previous-boot', boot_system)]:
        command(['nix-store', '--realise', target, '--add-root', run / name])
    git(config, 'worktree', 'add', '-b', task_branch, tree, baseline)
    host_file = tree / 'hosts' / args.host / 'default.nix'
    original = host_file.read_text()
    host_file.write_text(pin(original, revision))
    activated = False
    switched = False
    try:
        cfg = configuration(tree, args.host)
        manifest_ref = f'{tree}#nixosConfigurations.{args.host}.config.system.build.toplevel'
        command(['nix', 'build', *PURE, manifest_ref, '--out-link', run / 'system'], capture=False)
        system = (run / 'system').resolve(strict=True)
        manifest = json.loads((Path(cfg['package']) / 'share/vaultsyncd/release-manifest.json').read_text())
        if manifest['sourceInfo']['commit'] != revision:
            raise RuntimeError('构建包来源与目标 SHA 不一致')
        if (tree / 'flake.lock').read_bytes() != (run / 'host-flake.lock').read_bytes():
            raise RuntimeError('主机依赖锁发生变化')
        record.update(system=str(system), package=cfg['package'], user=cfg['user'], group=cfg['group'],
                      lock_sha256=hashlib.sha256((tree / 'flake.lock').read_bytes()).hexdigest())
        save()
        if args.build_only:
            record.update(passed=True, build_only=True)
            save()
            return
        # 不部署已经被另一会话修改的主机；build-only 不要求运行服务健康。
        clean(config)
        if git(config, 'rev-parse', 'HEAD') != baseline:
            raise RuntimeError('构建期间主机分支发生变化')
        before = health(old_cfg, verify_runtime=False)
        record['server_id'] = before['server_id']
        command(['sudo', '-n', 'nixos-rebuild', 'dry-activate', '--store-path', system], capture=False)
        activated = True
        command(['sudo', '-n', 'nixos-rebuild', 'test', '--store-path', system], capture=False)
        health(cfg, previous_id=before['server_id'])
        switched = True
        command(['sudo', '-n', 'nixos-rebuild', 'switch', '--store-path', system], capture=False)
        health(cfg, previous_id=before['server_id'])
        if Path('/run/current-system').resolve() != system or Path('/nix/var/nix/profiles/system').resolve() != system:
            raise RuntimeError('运行/启动 system 与已验收构建不一致')
        clean(config)
        if git(config, 'rev-parse', 'HEAD') != baseline or git(config, 'symbolic-ref', '--short', 'HEAD') != branch:
            raise RuntimeError('激活期间主机分支发生变化，保留部署工作树')
        if host_file.read_text() != original:
            git(tree, 'diff', '--check')
            (run / 'pin.diff').write_text(git(tree, 'diff'))
            git(tree, 'add', '--', host_file.relative_to(tree))
            git(tree, 'diff', '--cached', '--stat')
            git(tree, 'commit', '-m', f'chore(vaultsync): 固定本机服务版本至 {revision[:12]}',
                '-m', '固定已提交版本，保持主机依赖锁不变。已完成 pure 构建、test、HTTPS/IPC/身份与目录归属核对、switch 后健康检查。')
            git(config, 'merge', '--ff-only', task_branch)
        record.update(passed=True, host_commit=git(config, 'rev-parse', 'HEAD'))
        save()
    except BaseException:
        if activated:
            # 旧程序仍有 schema 围栏；不降级或修改数据库。若旧程序拒绝新 schema，需离线恢复备份。
            try:
                if switched:
                    command(['sudo', '-n', 'nixos-rebuild', 'boot', '--store-path', boot_system], capture=False)
                command(['sudo', '-n', 'nixos-rebuild', 'test', '--store-path', old_system], capture=False)
                health(old_cfg, previous_id=record.get('server_id'), verify_runtime=False)
                record['rollback_healthy'] = True
            except BaseException:
                record['rollback_healthy'] = False
        save()
        raise
    # 清理失败不能回滚已经验收并保存版本的部署。
    git(config, 'worktree', 'remove', tree)
    git(config, 'branch', '-d', task_branch)
    record['cleaned'] = True
    save()


if __name__ == '__main__':
    main()
