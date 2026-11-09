#!/usr/bin/env python3
"""S094 actual Nix release closure, empty-PATH native runtime and same-origin Web acceptance."""
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import sys
import time
from playwright.sync_api import sync_playwright, expect

VAULT=Path(__file__).resolve().parents[2]
PACKAGE=Path(sys.argv[1])
RUN=Path("/data")/f"vs-s094-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"

def command(args,**kwargs):
    result=subprocess.run([str(a) for a in args],text=True,capture_output=True,timeout=120,**kwargs)
    assert result.returncode==0,result.stdout+result.stderr
    return result.stdout

def main():
    command(["sudo","-n","mkdir","-m","755",RUN]);command(["sudo","-n","chown",f"{os.getuid()}:{os.getgid()}",RUN])
    manifest=json.loads((PACKAGE/"share/vaultsyncd/release-manifest.json").read_text())
    assert manifest["kind"]=="nix-component-set" and manifest["sourceInfo"]["commit"]==command(["git","rev-parse","HEAD"],cwd=VAULT).strip()
    for name,digest in manifest["inputs"].items():
        assert hashlib.sha256((VAULT/name).read_bytes()).hexdigest()==digest
    for item in manifest["files"]:
        path=PACKAGE/item["path"];assert path.stat().st_size==int(item["size"])
        assert hashlib.sha256(path.read_bytes()).hexdigest()==item["sha256"]
    for component in manifest["components"].values():
        for reference in component.values():
            assert len(reference["hash_groups"])==4 and all(len(part)==8 for part in reference["hash_groups"])
    closure=command(["nix-store","--query","--requisites",PACKAGE])
    (RUN/"closure.txt").write_text(closure)
    assert not any(re.search(r"-(nodejs|pnpm|typescript)-",Path(line).name) for line in closure.splitlines()),closure
    runtime={"PATH":"","HOME":str(RUN),"LANG":"C.UTF-8"}
    command([PACKAGE/"bin/vaultsyncd-server","--help"],env=runtime)
    command([PACKAGE/"bin/vaultsyncd-admin","--help"],env=runtime)
    missing=RUN/"missing-state";missing.mkdir(mode=0o700)
    invalid=RUN/"missing.toml";invalid.write_text(f'[server]\nlisten="127.0.0.1:0"\n[state]\ndir="{missing}"\n')
    bad=subprocess.run([str(PACKAGE/"bin/vaultsyncd-server"),"--config",str(invalid)],env={**runtime,"VAULTSYNCD_WEB_DIR":str(RUN/"no-web")},text=True,capture_output=True,timeout=10)
    assert bad.returncode==1 and "Web" in bad.stdout+bad.stderr
    assert not (missing/"server.sqlite").exists()
    state=RUN/"state";state.mkdir(mode=0o700);vaults=RUN/"vaults";vaults.mkdir()
    password=secrets.token_urlsafe(24)
    command([PACKAGE/"bin/vaultsyncd-admin","init","--state-dir",state],input=password+"\n",env=runtime)
    config=RUN/"service.toml";config.write_text(f'[server]\nlisten="127.0.0.1:0"\n[state]\ndir="{state}"\n[vaults]\nallowed_bases=["{vaults}"]\n[log]\nformat="json"\nlevel="info"\n')
    log=(RUN/"server.log").open("w")
    service=subprocess.Popen([str(PACKAGE/"bin/vaultsyncd-server"),"--config",str(config)],env=runtime,stdout=log,stderr=log)
    try:
        address=None
        for _ in range(150):
            for line in (RUN/"server.log").read_text().splitlines():
                fields=json.loads(line).get("fields",{})
                if fields.get("message")=="服务端开始监听":address=fields["listen"]
            if address:break
            assert service.poll() is None,(RUN/"server.log").read_text()
            time.sleep(.2)
        assert address
        connection=http.client.HTTPConnection(address)
        def get(path):
            connection.request("GET",path);r=connection.getresponse();return r.status,dict(r.getheaders()),r.read()
        status,headers,html=get("/");assert status==200 and "text/html" in headers["content-type"]
        assert headers["x-content-type-options"]=="nosniff"
        nonce=re.search(rb'<meta name="csp-nonce" content="([0-9a-f]{32})">',html).group(1).decode()
        assert f"'nonce-{nonce}'" in headers["content-security-policy"]
        assert "unsafe-inline" not in headers["content-security-policy"]
        for target in re.findall(rb'(?:src|href)="(\./assets/[^"]+)"',html):
            path="/"+target.decode().removeprefix("./")
            code,h,body=get(path);assert code==200 and body
            assert "javascript" in h["content-type"] if path.endswith(".js") else "text/css" in h["content-type"]
        code,_,body=get("/v1/health");assert code==200 and json.loads(body)["status"]=="ok"
        code,_,body=get("/v1/not-a-real-api");assert code==404 and b"<html" not in body
        code,_,_=get("/assets/missing.js");assert code==404
        connection.close()
        host,port=address.split(":");origin=f"http://localhost:{port}"
        with sync_playwright() as p:
            browser=p.chromium.launch(headless=True);page=browser.new_page()
            page.goto(origin+"/#/account")
            page.get_by_label(re.compile(r"^登录名(?:\s*\*)?$")).fill("owner");page.get_by_label(re.compile(r"^密码(?:\s*\*)?$")).fill(password)
            page.get_by_role("button",name="登录",exact=True).click()
            expect(page.get_by_text("已登录：owner",exact=True)).to_be_visible()
            page.get_by_role("link",name="备份管理",exact=True).click()
            expect(page.get_by_role("heading",name="一致备份",exact=True)).to_be_visible()
            page.screenshot(path=str(RUN/"web.png"),full_page=True)
            browser.close()
        (RUN/"result.json").write_text(json.dumps({"passed":True,"package":str(PACKAGE),"commit":manifest["sourceInfo"]["commit"],"checks":["manifest-inputs-and-file-hashes","grouped-build-identities","no-Node-pnpm-TypeScript-runtime-closure","empty-PATH-server-and-admin","missing-Web-fails-before-database","owner-init-without-Node","HTTP-static-and-assets-MIME","API-is-not-HTML-fallback","Chromium-same-origin-login-and-backup-page"]},ensure_ascii=False,indent=2))
        print("S094 Nix runtime acceptance passed: "+str(RUN))
    finally:
        if service.poll() is None:
            service.terminate()
            try:service.wait(timeout=20)
            except subprocess.TimeoutExpired:service.kill();service.wait()
        log.close()

if __name__=="__main__":main()
