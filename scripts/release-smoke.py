import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile

repo = Path(__file__).resolve().parent.parent
binary = Path(sys.argv[1]).resolve()
os_name = {'Darwin': 'apple-darwin', 'Linux': 'unknown-linux-musl'}[platform.system()]
arch = {'arm64': 'aarch64', 'aarch64': 'aarch64', 'x86_64': 'x86_64'}[platform.machine()]
filename = f'porchlight-{arch}-{os_name}.tar.gz'

with tempfile.TemporaryDirectory(prefix='porchlight-install-') as directory:
    root = Path(directory)
    mirror = root / 'mirror'
    mirror.mkdir()
    archive = mirror / filename
    with tarfile.open(archive, 'w:gz') as package:
        package.add(binary, arcname='porchlight')
    checksum = mirror / (filename + '.sha256')
    checksum.write_text(hashlib.sha256(archive.read_bytes()).hexdigest() + '  ' + filename + '\n')
    commands = root / 'commands'
    commands.mkdir()
    for name in ['sh', 'uname', 'sysctl', 'mktemp', 'rm', 'cut', 'sha256sum', 'shasum', 'tar', 'gzip', 'mkdir', 'cp', 'chmod', 'mv']:
        found = shutil.which(name)
        if found:
            (commands / name).symlink_to(found)
    curl = commands / 'curl'
    curl.write_text('#!' + sys.executable + '\nimport os, sys\nargs = [arg.replace("https://github.com/mahali00/porchlight/releases/latest/download/", ' + repr(mirror.as_uri() + '/') + ') for arg in sys.argv[1:]]\nos.execv(' + repr(shutil.which('curl')) + ', ["curl", *args])\n')
    curl.chmod(0o700)
    environment = {**os.environ, 'PATH': str(commands), 'PORCHLIGHT_INSTALL_DIR': str(root / 'bin'), 'PORCHLIGHT_HOME': str(root / 'state')}
    subprocess.run(['/bin/sh', str(repo / 'install.sh')], env=environment, check=True)
    installed = root / 'bin' / 'porchlight'
    runtime = {**os.environ, 'PORCHLIGHT_HOME': str(root / 'state')}
    def run(*args):
        result = subprocess.run([str(installed), *args], env=runtime, text=True, capture_output=True, check=True)
        return result.stdout
    print(run('--version').strip())
    assert json.loads(run('status', '--json'))['running'] is False
    run('tools', 'add', 'today', '--read-only', '--', 'date', '+%F')
    assert json.loads(run('tools', 'list', '--json'))['ok']
    assert json.loads((root / 'state' / 'porchlight.json').read_text())['commands']['today']
    subprocess.run([str(installed), 'completions', 'bash'], env=runtime, stdout=subprocess.DEVNULL, check=True)
    before = hashlib.sha256(installed.read_bytes()).hexdigest()
    checksum.write_text('0' * 64 + '  ' + filename + '\n')
    rejected = subprocess.run(['/bin/sh', str(repo / 'install.sh')], env=environment, capture_output=True, text=True)
    assert rejected.returncode != 0 and "doesn't match its checksum" in rejected.stderr
    assert hashlib.sha256(installed.read_bytes()).hexdigest() == before
    print('Packaged installation, fresh-state CLI, completions and checksum rejection passed.')
