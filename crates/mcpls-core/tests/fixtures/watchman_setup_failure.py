#!/usr/bin/python3
"""Inject controlled Watchman setup transport or response failures."""
import json
from pathlib import Path
import socket
import sys

settings = json.loads(Path(__file__).with_suffix('.json').read_text())
logfile = Path(sys.argv[sys.argv.index('--logfile') + 1])
listener = socket.socket(socket.AF_UNIX)
listener.bind(sys.argv[sys.argv.index('--sockname') + 1])
listener.listen(1)
connection, _ = listener.accept()
reader = connection.makefile('rb')
clocks = 0
while line := reader.readline():
    command = json.loads(line)
    method = command[0]
    response = {'watch': command[1]} if method == 'watch' else (
        {'clock': 'c:1:1'} if method == 'clock' else {'subscribe': command[2]}
    )
    if method == 'clock':
        clocks += 1
    stage = 'barrier' if method == 'clock' and clocks == 2 else method
    if stage == settings['fail_after']:
        logfile.write_text(settings['message'])
        if settings.get('failure', 'write') == 'response':
            response = {}
        else:
            # The final reply stays readable, but the next client write fails.
            connection.shutdown(socket.SHUT_RD)
        connection.sendall((json.dumps(response) + '\n').encode())
        break
    connection.sendall((json.dumps(response) + '\n').encode())
connection.close()
listener.close()
