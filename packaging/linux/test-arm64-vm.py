#!/usr/bin/env python3
"""Run the container acceptance in the already prepared, dedicated ARM64 TCG VM.
Args: VM directory, verified local ARM64 image, evidence directory.
The VM's initial provisioning/serial-checked blank disk is recorded separately.
"""
import importlib.util
import json
import os
from pathlib import Path
import secrets
import subprocess
import sys
import traceback
sys.dont_write_bytecode=True

VM=Path(sys.argv[1]).resolve(strict=True)
IMAGE=sys.argv[2]
EVIDENCE=Path(sys.argv[3]).resolve(strict=True)
assert VM.name=='ubuntu-arm64-tcg' and VM.is_relative_to(EVIDENCE)
assert IMAGE.startswith('localhost/vaultsyncd-s098:') and IMAGE.endswith('-arm64')

def main():
    meta=json.loads((VM/'vm.json').read_text())
    cmdline=Path(f'/proc/{meta["pid"]}/cmdline').read_bytes()
    assert b'vaultsyncd-s098-arm64-tcg' in cmdline and str(VM/'root.qcow2').encode() in cmdline
    common=['-o','BatchMode=yes','-o','ConnectTimeout=30','-o','StrictHostKeyChecking=yes',
            '-o','UserKnownHostsFile='+str(VM/'known_hosts'),'-i',str(VM/'id_ed25519')]
    ssh=['ssh',*common,'-p',str(meta['ssh_port']),'ubuntu@127.0.0.1']
    scp=['scp',*common,'-P',str(meta['ssh_port'])]
    archive=EVIDENCE/('arm64-image-'+str(os.getpid())+'.tar')
    subprocess.run(['podman','save','--format','docker-archive','-o',archive,IMAGE],check=True)
    script=Path(__file__).with_name('test-container.py')
    subprocess.run([*ssh,'mkdir -p /home/ubuntu/s098; sudo -n mkdir -p /data/s098-acceptance'],check=True)
    subprocess.run([*scp,str(archive),str(script),'ubuntu@127.0.0.1:/home/ubuntu/s098/'],check=True)
    with (VM/'current-image-load.log').open('w') as log:
        subprocess.run([*ssh,'sudo -n podman load -i /home/ubuntu/s098/'+archive.name],check=True,stdout=log,stderr=subprocess.STDOUT,timeout=240)
    password=secrets.token_urlsafe(24)  # Retained only in these two process memories.
    command='sudo -n env VAULTSYNCD_TEST_EXTERNAL_BROWSER=stdio VAULTSYNCD_TEST_PASSWORD_STDIN=1 VAULTSYNCD_TEST_ISOLATED_VM=1 python3 /home/ubuntu/s098/test-container.py '+IMAGE+' arm64 /data/s098-acceptance'
    report=[]
    with (VM/'guest-acceptance.stderr.log').open('w') as errors:
        process=subprocess.Popen([*ssh,command],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=errors,text=True,bufsize=1)
        process.stdin.write(password+'\n');process.stdin.flush()
        for line in process.stdout:
            value=json.loads(line)
            if value.get('stage')=='browser_pending':
                spec=importlib.util.spec_from_file_location('portable_acceptance',script)
                helper=importlib.util.module_from_spec(spec);spec.loader.exec_module(helper)
                helper.configure(IMAGE,'arm64',EVIDENCE)
                helper.RUN=VM/'host-chromium';helper.RUN.mkdir(exist_ok=True)
                helper.browser(password,value['root'])
                proof={'passed':True,'root':value['root'],'browser':helper.RESULT['browser']}
                (VM/'host-chromium-result.json').write_text(json.dumps({**proof,'refresh_attempts':helper.RESULT['browser_refresh_attempts']},indent=2))
                process.stdin.write(json.dumps(proof)+'\n');process.stdin.flush()
            else:
                report.append(value)
                print(json.dumps(value),flush=True)
        process.stdin.close()
        code=process.wait(timeout=30)
    assert code==0 and report[-1]['passed']
    guest_run=report[-1]['run']
    assert guest_run.startswith('/data/s098-acceptance/accept-arm64-') and '..' not in guest_run
    subprocess.run([*scp,'ubuntu@127.0.0.1:'+guest_run+'/result.json',str(VM/'container-result.json')],check=True)
    result=json.loads((VM/'container-result.json').read_text())
    assert result['process']['uname']=='aarch64' and result['passed']
    password=None
    print(json.dumps({'passed':True,'mode':'actual Ubuntu ARM64 kernel in full-system TCG','guest_run':guest_run,'checks':result['checks']}))

if __name__=='__main__':
    try:main()
    except Exception as error:
        print(json.dumps({'passed':False,'error_type':type(error).__name__,'frames':[{'file':Path(f.filename).name,'line':f.lineno,'function':f.name} for f in traceback.extract_tb(error.__traceback__) if 'test-' in f.filename]}))
        sys.exit(1)
