#!/usr/bin/env python3
"""S090 real Chromium, isolated CoW btrfs service, streamed export and CLI verification."""
import contextlib
import functools
import hashlib
import http.client
import http.server
import json
import os
import re
from pathlib import Path
import secrets
import subprocess
import threading
import time
from csp_support import CspStaticFiles, field_label
from playwright.sync_api import sync_playwright, expect

REPO=Path(__file__).resolve().parents[2]
BIN=Path(os.environ.get("VAULTSYNCD_TEST_BIN_DIR",REPO/"target/debug"))
RUN=Path("/data")/f"vs-s090-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"

def checked(args,**kwargs):
    result=subprocess.run([str(arg) for arg in args],capture_output=True,text=True,**kwargs)
    assert result.returncode==0, result.stderr
    return result.stdout

@contextlib.contextmanager
def volume():
    image=RUN/"volume.img";image.touch(mode=0o600)
    with image.open("wb") as target:target.truncate(1024*1024*1024)
    loop=None;mounted=False;mount=RUN/"mnt";mount.mkdir()
    try:
        loop=checked(["sudo","-n","losetup","--find","--show",image]).strip()
        checked(["sudo","-n","mkfs.btrfs","-q",loop])
        checked(["sudo","-n","mount","-o","user_subvol_rm_allowed",loop,mount]);mounted=True
        checked(["sudo","-n","chown",f"{os.getuid()}:{os.getgid()}",mount])
        assert checked(["findmnt","-no","FSTYPE","-T",mount]).strip()=="btrfs"
        yield mount
    finally:
        if mounted:checked(["sudo","-n","umount",mount])
        if loop:checked(["sudo","-n","losetup","-d",loop])
        image.unlink()

def main():
    checked(["sudo","-n","mkdir","-m","755",RUN]);checked(["sudo","-n","chown",f"{os.getuid()}:{os.getgid()}",RUN])
    with volume() as mount:
        state=mount/"state";roots=mount/"roots";state.mkdir(mode=0o700);roots.mkdir()
        candidate=roots/"documents";candidate.mkdir()
        sample=candidate/"keep.txt";sample.write_bytes(b"S090 confirmed bytes\n")
        (candidate/"large.bin").write_bytes(b"A"*(8*1024*1024))
        original=hashlib.sha256(sample.read_bytes()).hexdigest()
        password=secrets.token_urlsafe(24)
        checked([BIN/"vaultsyncd-admin","init","--state-dir",state],input=password+"\n")
        config=RUN/"server.toml";config.write_text(f'[server]\nlisten="127.0.0.1:0"\n[state]\ndir="{state}"\n[roots]\nallowed_bases=["{roots}"]\n[log]\nformat="json"\nlevel="info"\n')
        log=(RUN/"server.log").open("w")
        server=subprocess.Popen([str(BIN/"vaultsyncd-server"),"--config",str(config)],stdout=log,stderr=log)
        proxy=None
        try:
            address=None
            for _ in range(150):
                for line in (RUN/"server.log").read_text().splitlines():
                    fields=json.loads(line).get("fields",{})
                    if fields.get("message")=="服务端开始监听":address=fields["listen"]
                if address:break
                assert server.poll() is None
                time.sleep(.2)
            assert address
            fault=[None];prepare_drop=[True];requests=[]
            class Handler(CspStaticFiles):
                def log_message(self,*_):pass
                def do_POST(self):self.forward()
                def do_GET(self):
                    if self.path.startswith("/v1/"):self.forward()
                    else:super().do_GET()
                def forward(self):
                    backend=http.client.HTTPConnection(address,timeout=60)
                    body=self.rfile.read(int(self.headers.get("content-length","0")))
                    headers={k:v for k,v in self.headers.items() if k.lower() not in ("connection","transfer-encoding")}
                    try:
                        backend.request(self.command,self.path,body,headers)
                        response=backend.getresponse();payload=response.read()
                        requests.append((self.command,self.path,response.status))
                        self.send_response(response.status)
                        for key,value in response.getheaders():
                            if key.lower() not in ("connection","transfer-encoding","content-length"):self.send_header(key,value)
                        self.send_header("content-length",str(len(payload)));self.end_headers()
                        if self.path=="/v1/admin/backup-snapshots" and self.command=="POST" and prepare_drop[0]:
                            prepare_drop[0]=False;self.close_connection=True;return
                        mode=fault[0] if self.path.endswith("/export") else None
                        if mode=="truncate":
                            fault[0]=None;self.wfile.write(payload[:100000]);self.wfile.flush();self.close_connection=True;return
                        if mode=="slow":
                            fault[0]=None
                            for at in range(0,len(payload),65536):
                                self.wfile.write(payload[at:at+65536]);self.wfile.flush();time.sleep(.05)
                        else:self.wfile.write(payload)
                    except (BrokenPipeError,ConnectionResetError):pass
                    finally:backend.close()
            proxy=http.server.ThreadingHTTPServer(("127.0.0.1",0),functools.partial(Handler,directory=str(REPO/"apps/web/dist")))
            threading.Thread(target=proxy.serve_forever,daemon=True).start()
            origin=f"http://localhost:{proxy.server_port}"
            with sync_playwright() as p:
                browser=p.chromium.launch(headless=True)
                context=browser.new_context(accept_downloads=True);page=context.new_page()
                page.add_init_script("""window.__cspViolations=[];document.addEventListener('securitypolicyviolation',event=>{window.__cspViolations.push({directive:event.effectiveDirective,blocked:event.blockedURI});});""")
                response=page.goto(origin+"/#/account")
                assert response
                nonce=page.locator('meta[name="csp-nonce"]').get_attribute("content")
                assert nonce and f"'nonce-{nonce}'" in response.headers.get("content-security-policy", "")
                page.get_by_label(field_label("登录名")).fill("owner");page.get_by_label(field_label("密码")).fill(password)
                page.get_by_role("button",name="登录",exact=True).click();expect(page.get_by_text("已登录：owner",exact=True)).to_be_visible()
                def api(path,body=None):
                    return page.evaluate("""async ([path,body])=>{
                      const r=await fetch(path,{method:body?'POST':'GET',headers:body?{'content-type':'application/json'}:undefined,body:body?JSON.stringify(body):undefined});
                      const v=await r.json();if(!r.ok)throw new Error(JSON.stringify(v));return v;
                    }""",[path,body])
                root=api("/v1/admin/roots",{"path":str(candidate)})["root_id"]
                for _ in range(150):
                    entries=api(f"/v1/roots/{root}/recovery")["items"]
                    if len(entries)==2:break
                    time.sleep(.2)
                else:raise AssertionError("observer did not capture source")
                page.get_by_role("link",name="备份管理",exact=True).click()
                prepare=page.get_by_role("button",name="准备一致备份（未知时重试同ID）",exact=True)
                prepare.click();expect(page.get_by_role("alert")).to_be_visible()
                prepare.click()
                download=page.get_by_role("button",name="导出或重试缺失下载",exact=True)
                expect(download).to_be_enabled(timeout=30000)
                snapshots=api("/v1/admin/backup-snapshots")["items"];assert len(snapshots)==1
                backup_id=snapshots[0]["backup_id"]
                page.get_by_role("button",name="校验服务端源",exact=True).click()
                expect(page.get_by_text("源已校验（不代表已下载）",exact=True)).to_be_visible(timeout=30000)
                page.get_by_role("button",name="查看固定清单",exact=True).click()
                expect(page.get_by_role("heading",name="固定清单",exact=True)).to_be_visible()
                fault[0]="truncate";download.click()
                expect(page.get_by_text("未完成，不能作为有效备份",exact=True)).to_be_visible(timeout=20000)
                expect(page.get_by_role("alert")).to_be_visible()
                assert page.get_by_text("归档完整性已验证",exact=True).count()==0
                fault[0]="slow";download.click()
                cancel=page.get_by_role("button",name="取消下载并保留未完成状态",exact=True)
                expect(cancel).to_be_visible();cancel.click()
                expect(page.get_by_text("下载已取消；部分暂存保留，不能作为有效备份",exact=True)).to_be_visible()
                download.click()
                expect(page.get_by_text("已下载，尚未重读校验",exact=True)).to_be_visible(timeout=30000)
                assert len(api("/v1/admin/backup-snapshots")["items"])==1
                page.get_by_role("button",name="重读校验浏览器暂存",exact=True).click()
                expect(page.get_by_text("归档完整性已验证",exact=True)).to_be_visible(timeout=30000)
                page.reload()
                expect(page.get_by_text("已下载，尚未重读校验",exact=True)).to_be_visible(timeout=10000)
                page.get_by_role("button",name="选择备份",exact=True).click()
                page.get_by_role("button",name="重读校验浏览器暂存",exact=True).click()
                expect(page.get_by_text("归档完整性已验证",exact=True)).to_be_visible(timeout=30000)
                archive=RUN/f"backup-{backup_id}.vsbk";sidecar=Path(str(archive)+".verification.json")
                with page.expect_download() as event:page.get_by_role("link",name="另存已校验归档",exact=True).click()
                event.value.save_as(archive);archive.chmod(0o600)
                with page.expect_download() as event:page.get_by_role("link",name="下载完整性收据",exact=True).click()
                event.value.save_as(sidecar);sidecar.chmod(0o600)
                assert json.loads(sidecar.read_text())["state"]=="exported_unverified"
                verified=json.loads(checked([BIN/"vaultsyncd-admin","backup","verify",archive,"--json"]))
                assert verified["state"]=="verified"
                bad=RUN/"missing.vsbk";bad.write_bytes(archive.read_bytes()[:-4]);bad.chmod(0o600)
                page.get_by_label(field_label("选择另存归档再次验证")).set_input_files(bad)
                expect(page.get_by_text("未完成，不能作为有效备份",exact=True)).to_be_visible()
                page.get_by_label(field_label("选择另存归档再次验证")).set_input_files(archive)
                expect(page.get_by_text("归档完整性已验证",exact=True)).to_be_visible(timeout=30000)
                release=page.get_by_role("button",name="释放服务端快照",exact=True)
                release.click()
                dialog=page.get_by_role("dialog",name="释放此备份源？",exact=True)
                expect(dialog).to_be_visible()
                expect(dialog.get_by_role("button",name="关闭",exact=True)).to_be_focused()
                # Traverse beyond all controls in both directions to exercise the wrap.
                for key in ("Tab","Shift+Tab"):
                    for _ in range(dialog.get_by_role("button").count()+2):
                        page.keyboard.press(key)
                        assert dialog.evaluate("element => element.contains(document.activeElement)"), f"focus escaped release dialog via {key}"
                release_posts_before=[request for request in requests if request[0]=="POST" and request[1].endswith("/release")]
                page.keyboard.press("Escape")
                expect(dialog).not_to_be_visible()
                expect(release).to_be_focused()
                assert api(f"/v1/admin/backup-snapshots/{backup_id}")["phase"]=="ready", "Escape released the snapshot"
                assert [request for request in requests if request[0]=="POST" and request[1].endswith("/release")]==release_posts_before
                release.click()
                expect(dialog).to_be_visible()
                dialog.get_by_role("button",name="确认释放此快照",exact=True).click()
                for _ in range(100):
                    status=api(f"/v1/admin/backup-snapshots/{backup_id}")
                    if status["phase"]=="released" and not status["job_pending"]:break
                    time.sleep(.2)
                else:raise AssertionError(status)
                expect(page.get_by_text(backup_id+" · 已释放 · 作业已结束",exact=True)).to_be_visible(timeout=10000)
                assert hashlib.sha256(sample.read_bytes()).hexdigest()==original
                assert archive.is_file() and json.loads(sidecar.read_text())["state"]=="verified"
                page.screenshot(path=str(RUN/"backup.png"),full_page=True)
                assert page.evaluate("localStorage.length")==0
                violations=page.evaluate("window.__cspViolations")
                (RUN/"csp-violations.json").write_text(json.dumps(violations,ensure_ascii=False,indent=2))
                assert violations==[], f"production CSP blocked backup UI resources: {violations}"
                context.close();browser.close()
                (RUN/"result.json").write_text(json.dumps({"passed":True,"backup_id":backup_id,"checks":["unknown-prepare-retries-same-id","real-btrfs-fixed-source","source-verified-is-not-downloaded","bounded-manifest-view","truncated-download-incomplete","cancel-retains-incomplete","retry-without-new-prepare","OPFS-reread-verified","reload-requires-reread","downloaded-archive-and-unverified-receipt","Rust-CLI-verifies-browser-archive","selected-truncated-file-withdraws-verification","selected-complete-file-verifies","release-keeps-original-and-archive","release-dialog-tab-and-shift-tab-trap","escape-cancels-and-restores-focus","production-csp-no-violations"],"verified":verified},ensure_ascii=False,indent=2))
            print(f"S090 browser acceptance passed: {RUN}")
        finally:
            if proxy:proxy.shutdown();proxy.server_close()
            if server.poll() is None:
                server.terminate()
                try:server.wait(timeout=20)
                except subprocess.TimeoutExpired:server.kill();server.wait()
            log.close()
    assert checked(["findmnt","-no","TARGET","-T",RUN/"mnt"]).strip()=="/data"

if __name__=="__main__":main()
