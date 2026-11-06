#!/usr/bin/env python3
"""Guard the shipped Compose entry before any engine resource is created.
Some Docker-compatible engines ignore bind.create_host_path=false. Never let a
mistyped state/Root source silently become a new directory on another filesystem.
"""
import argparse
import os
from pathlib import Path
import shutil
import stat

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--project-name',required=True)
    parser.add_argument('action',choices=['up','create','start','restart','stop','down','ps','logs','config'])
    args=parser.parse_args()
    if args.action in ('up','create','start','restart'):
        for key in ('VAULTSYNCD_STATE','VAULTSYNCD_ROOTS'):
            raw=os.environ.get(key,'')
            path=Path(raw)
            if not raw or not path.is_absolute():parser.error(key+' must name an existing absolute bind source')
            try:mode=path.lstat().st_mode
            except FileNotFoundError:parser.error(key+' bind source does not exist; no directory created')
            if not stat.S_ISDIR(mode) or path.is_symlink():parser.error(key+' must be a real directory')
    engine=shutil.which('docker-compose')
    if engine is None:parser.error('docker-compose is required')
    flags=['-d','--pull','never'] if args.action=='up' else []
    if args.action=='create':flags=['--pull','never']
    if args.action=='config':flags=['--format','json']
    command=[engine,'-p',args.project_name,'-f',str(Path(__file__).with_name('compose.yaml')),args.action,*flags]
    os.execv(engine,command)

if __name__=='__main__':main()
