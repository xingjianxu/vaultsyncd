#!/usr/bin/env python3
"""Continue the recorded native fixture using real complete cache recovery, never fake residency."""
import hashlib, importlib.util, json, os
from pathlib import Path
import subprocess, sys, time, uuid
sys.dont_write_bytecode = True
ROOT=Path(__file__).resolve().parents[3]
spec=importlib.util.spec_from_file_location("s100_native",ROOT/"tests/platform/windows/s100-maintenance.py")
native=importlib.util.module_from_spec(spec);spec.loader.exec_module(native)
SSH,SCP,USER,HOST=native.SSH,native.SCP,native.USER,native.HOST
checked=native.checked

def main():
    prior=Path(sys.argv[1]).resolve();data=json.loads((prior/"checkpoint.json").read_text())
    component=ROOT/"artifacts/S100/client-1794752";receipt=json.loads((component/"receipt.json").read_text())
    source=receipt["source"]["commit"]
    assert len(source)==40 and all(c in "0123456789abcdef" for c in source)
    checked(["git","merge-base","--is-ancestor",source,"HEAD"])
    assert checked(["git","rev-parse",source+"^{tree}"]).strip()==receipt["source"]["tree"]
    for item in receipt["files"]:
        path=(component/"payload"/item["path"]).resolve()
        assert path.is_relative_to((component/"payload").resolve())
        raw=path.read_bytes();assert len(raw)==item["bytes"] and hashlib.sha256(raw).hexdigest()==item["sha256"]
    run_id=f"s100-cont-{int(time.time())}-{os.getpid()}";run=ROOT/"artifacts/S100"/run_id;run.mkdir()
    guest=f"C:/vaultsyncd-lab/{run_id}";results=guest+"/results"
    summary={"ok":False,"prior_run":prior.name,"source_commit":receipt["source"]["commit"],"materialization_kind":"manual recovery of verified actual fixed-version cache; no network success claimed"}
    fixture={"source_commit":receipt["source"]["commit"],"cache_file":data["enroll"]["cache"]+"\\download-98b7c0e401f406ca330a2779eca650ab","cache_bytes":1048576,"cache_sha256":hashlib.sha256(b"C"*1048576).hexdigest()}
    (run/"fixture.json").write_text(json.dumps(fixture));(run/"prior.json").write_text(json.dumps(data))
    for name,source in [("s100-continuation.ps1",Path(__file__).with_suffix(".ps1")),("s099-logon-secrets.ps1",ROOT/"tests/platform/windows/s099-logon-secrets.ps1")]:
        (run/name).write_text(source.read_text(),encoding="utf-8-sig")
    (run/"hashes.json").write_text(json.dumps({p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in run.glob("*.ps1")}))
    def ps(mode):return checked([*SSH,f"powershell -NoProfile -ExecutionPolicy Bypass -File {guest}/s100-continuation.ps1 -Mode {mode}"])
    def read(name):
        command=f"[Console]::OutputEncoding=New-Object Text.UTF8Encoding($false); if(Test-Path '{results}/{name}'){{Get-Content -Raw -Encoding UTF8 '{results}/{name}'}}else{{exit 3}}"
        return checked([*SSH,'powershell -NoProfile -Command "'+command+'"'])
    def wait(label,test,seconds=300):
        end=time.monotonic()+seconds;last=None
        while time.monotonic()<end:
            try:
                value=test()
                if value:return value
            except (RuntimeError,OSError,ValueError) as e:last=str(e)
            time.sleep(.3)
        raise RuntimeError(f"timeout {label}: {last}")
    def ready(name):
        def poll():
            try:error=read("worker-error.json")
            except RuntimeError:error=""
            if error:raise AssertionError(error)
            return json.loads(read(name))
        return wait(name,poll)
    def action(name):
        ident=uuid.uuid4().hex;(run/"request.json").write_text(json.dumps({"id":ident,"action":name}))
        checked([*SCP,run/"request.json",f"{USER}@{HOST}:{results}/request.tmp"])
        command=f"Move-Item -Force '{results}/request.tmp' '{results}/request.json'"
        checked([*SSH,'powershell -NoProfile -Command "'+command+'"'])
        value=wait(name,lambda:(x if (x:=ready("response.json"))["id"]==ident else None))
        summary[name]=value;(run/"checkpoint.json").write_text(json.dumps(summary,indent=2,ensure_ascii=False));print("NATIVE_CONTINUATION_PASSED",name,flush=True);return value
    def console():return checked([*SSH,'powershell -NoProfile -Command "(Get-CimInstance Win32_ComputerSystem).UserName"']).strip()
    def boot():return checked([*SSH,"powershell -NoProfile -Command \"(Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToString('o')\""]).strip()
    def reboot(label):
        before=boot();print("CONTROLLED_REBOOT",label,flush=True)
        try:subprocess.run([*SSH,'powershell -NoProfile -Command "Restart-Computer -Force"'],capture_output=True,timeout=30)
        except subprocess.TimeoutExpired:pass
        after=wait("actual boot",lambda:(x if(x:=boot())!=before else None),420);return {"before":before,"after":after}
    prepared=False;restored=False
    try:
        command=f"New-Item -ItemType Directory '{guest}'|Out-Null"
        checked([*SSH,'powershell -NoProfile -Command "'+command+'"'])
        checked([*SCP,*[p for p in run.iterdir() if p.is_file()],f"{USER}@{HOST}:{guest}/"])
        summary["prior_console_user"]=console();prepared=True;print(ps("Prepare"),flush=True)
        summary["retained_user_boot"]=reboot("retained user continuation")
        summary["ready"]=ready("ready.json")
        restored_file=action("materialize-cache");assert restored_file["cache"]["sha256"]==fixture["cache_sha256"]
        removed=action("provider-uninstall");assert removed["cloud_metadata"][0]==0 and removed["root_hresult"]<0 and removed["state_retained"] and removed["cache_retained"]
        final=action("finish");assert final["processes"]==[]
        print(ps("Restore"),flush=True);restored=True;summary["restored_boot"]=reboot("restore original Quickemu")
        summary["restored_console_user"]=wait("original console",lambda:(x if(x:=console())==summary["prior_console_user"] else None))
        summary["native_ok"]=True
    finally:
        if prepared and not restored:
            try:
                print(ps("Restore"),flush=True);reboot("restore original after continuation failure")
                summary["restored_console_user"]=wait("original console",lambda:(x if(x:=console())==summary["prior_console_user"] else None))
            except Exception as e:summary["restore_error"]=str(e)
        if prepared:
            try:checked([*SCP,"-r",f"{USER}@{HOST}:{results}",run/"guest-results"])
            except Exception as e:summary["collection_error"]=str(e)
        (run/"result.json").write_text(json.dumps(summary,ensure_ascii=False,indent=2));print("ARTIFACTS",run,flush=True)
    assert summary.get("native_ok")
    count=0;dirty=False;expected=hashlib.sha256(b"S100 unconfirmed dirty bytes").hexdigest()
    for path in (run/"guest-results/recovery-exports").rglob("manifest.json"):
        manifest=json.loads(path.read_text())
        for item in manifest["files"]:
            assert Path(item["name"]).name==item["name"] and item["name"] not in (".","..")
            raw=(path.parent/item["name"]).read_bytes();assert len(raw)==item["size"] and hashlib.sha256(raw).hexdigest()==item["sha256"]
            dirty |= item["source"] is not None and item["sha256"]==expected;count+=1
    assert dirty and count>0
    summary["ok"]=True;summary["independently_read_and_hashed_export_files"]=count
    (run/"result.json").write_text(json.dumps(summary,ensure_ascii=False,indent=2));print("PASS native provider removal and independent recovery readback",count,flush=True)
if __name__=="__main__":main()
