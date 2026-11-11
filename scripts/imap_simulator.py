import asyncio
import base64
import time
import asyncio.sslproto
import email.parser
import email.policy
import os
import re
import shlex
import ssl
from collections import defaultdict
from datetime import datetime, timezone

MAIL = defaultdict(lambda: defaultdict(dict))
NEXT = defaultdict(lambda: 1)
WATCH = defaultdict(set)


def oauth_identity(encoded):
    try:
        fields = base64.b64decode(encoded, validate=True).decode().split('\x01')
        if len(fields) != 4 or not fields[0].startswith('user=') or not fields[1].startswith('auth=Bearer ') or fields[2:] != ['', '']:
            return None
        lifetime = int(os.environ.get('SIMULATOR_OAUTH_TOKEN_SECONDS', '60'))
        epoch = int(time.time()) // max(1, lifetime)
        if fields[1] != f'auth=Bearer simulator:{epoch}':
            return None
        return fields[0][5:] or None
    except (ValueError, UnicodeError):
        return None


def quoted(value):
    if value is None:
        return b'NIL'
    return ('"' + str(value).replace('\\', '\\\\').replace('"', '\\"').replace('\r', '').replace('\n', '') + '"').encode()


def selected_ids(expression, messages):
    maximum = max(messages, default=0)
    result = set()
    for part in expression.split(','):
        bounds = part.split(':')
        values = [maximum if value == '*' else int(value) for value in bounds]
        low, high = min(values), max(values)
        result.update(uid for uid in messages if low <= uid <= high)
    return sorted(result)


def changed(key):
    count = len(MAIL[key[0]][key[1]])
    for writer in list(WATCH[key]):
        if not writer.is_closing():
            writer.write(f'* {count} EXISTS\r\n'.encode())


def append(user, folder, raw, flags=None):
    key = user, folder
    uid = NEXT[key]
    NEXT[key] += 1
    headers = email.parser.BytesHeaderParser(policy=email.policy.default).parsebytes(raw)
    MAIL[user][folder][uid] = {'raw': raw, 'flags': flags or [], 'headers': headers, 'date': datetime.now(timezone.utc).strftime('%d-%b-%Y %H:%M:%S +0000')}
    changed(key)
    return uid


async def connection(reader, writer):
    user, folder, idle_tag = None, 'INBOX', None
    writer.write(b'* OK lightweight capacity simulator\r\n')
    try:
        while line := await reader.readline():
            line = line.decode().rstrip('\r\n')
            if idle_tag:
                if line.upper() != 'DONE':
                    raise ValueError('expected DONE')
                WATCH[user, folder].discard(writer)
                writer.write(f'{idle_tag} OK IDLE completed\r\n'.encode())
                idle_tag = None
                await writer.drain()
                continue
            tokens = shlex.split(line)
            tag, command = tokens[:2]
            command = command.upper()
            arguments = tokens[2:]
            response = b''
            if command == 'CAPABILITY':
                response = b'* CAPABILITY IMAP4rev1 IDLE MOVE UIDPLUS AUTH=XOAUTH2\r\n'
            elif command == 'AUTHENTICATE':
                if not arguments or arguments[0].upper() != 'XOAUTH2':
                    writer.write(f'{tag} NO unsupported authentication\r\n'.encode())
                    await writer.drain()
                    continue
                if len(arguments) > 1:
                    encoded = arguments[1].encode()
                else:
                    writer.write(b'+ \r\n')
                    await writer.drain()
                    encoded = (await reader.readline()).strip()
                user = oauth_identity(encoded)
                if user is None:
                    writer.write(b'+ eyJzdGF0dXMiOiI0MDEifQ==\r\n')
                    await writer.drain()
                    await reader.readline()
                    writer.write(f'{tag} NO authentication rejected\r\n'.encode())
                    await writer.drain()
                    continue
            elif command == 'LOGIN':
                user = arguments[0]
                if arguments[1] != 'soak-password':
                    writer.write(f'{tag} NO authentication rejected\r\n'.encode())
                    await writer.drain()
                    continue
            elif command == 'LOGOUT':
                writer.write(f'* BYE logout\r\n{tag} OK logout\r\n'.encode())
                await writer.drain()
                break
            elif user is None:
                writer.write(f'{tag} NO authenticate first\r\n'.encode())
                await writer.drain()
                continue
            elif command in ('SELECT', 'EXAMINE'):
                folder = arguments[0]
                response = f'* FLAGS (\\Seen \\Answered \\Flagged \\Draft)\r\n* {len(MAIL[user][folder])} EXISTS\r\n* 0 RECENT\r\n* OK [UIDVALIDITY 1]\r\n* OK [UIDNEXT {NEXT[user, folder]}]\r\n'.encode()
            elif command == 'IDLE':
                idle_tag = tag
                WATCH[user, folder].add(writer)
                writer.write(b'+ idling\r\n')
                await writer.drain()
                continue
            elif command == 'LIST':
                for name in ('INBOX', 'Trash', 'Sent', 'Archive'):
                    special = '\\Trash' if name == 'Trash' else '\\Sent' if name == 'Sent' else ''
                    response += b'* LIST (' + special.encode() + b') "/" ' + quoted(name) + b'\r\n'
            elif command == 'STATUS':
                name = arguments[0]
                messages = MAIL[user][name]
                response = b'* STATUS ' + quoted(name) + f' (MESSAGES {len(messages)} UIDNEXT {NEXT[user, name]} UIDVALIDITY 1 UNSEEN {sum("\\Seen" not in m["flags"] for m in messages.values())})\r\n'.encode()
            elif command == 'APPEND':
                destination = arguments[0]
                match = re.search(r'\{(\d+)(\+)?\}$', line)
                if not match or int(match[1]) > 26214400:
                    raise ValueError('invalid literal')
                if not match[2]:
                    writer.write(b'+ ready for literal\r\n')
                    await writer.drain()
                raw = await reader.readexactly(int(match[1]))
                if await reader.readexactly(2) != b'\r\n':
                    raise ValueError('invalid literal terminator')
                uid = append(user, destination, raw)
                writer.write(f'{tag} OK [APPENDUID 1 {uid}] appended\r\n'.encode())
                await writer.drain()
                continue
            elif command == 'UID':
                operation = arguments[0].upper()
                messages = MAIL[user][folder]
                if operation == 'SEARCH':
                    ids = selected_ids(arguments[2], messages) if arguments[1].upper() == 'UID' else sorted(messages)
                    response = ('* SEARCH' + ''.join(' ' + str(uid) for uid in ids) + '\r\n').encode()
                elif operation == 'FETCH':
                    for uid in selected_ids(arguments[1], messages):
                        message = messages[uid]
                        sequence = sorted(messages).index(uid) + 1
                        flags = ' '.join(message['flags'])
                        partial = re.search(r'BODY\.PEEK\[\](?:<(\d+)\.(\d+)>)?', line, re.I)
                        if partial:
                            start = int(partial[1] or 0)
                            raw = message['raw'][start:start + int(partial[2])] if partial[2] else message['raw']
                            section = f'BODY[]<{start}>' if partial[1] else 'BODY[]'
                            response += f'* {sequence} FETCH (UID {uid} {section} {{{len(raw)}}}\r\n'.encode() + raw + b')\r\n'
                        elif 'ENVELOPE' in line.upper():
                            headers = message['headers']
                            envelope = b'(' + quoted(headers.get('Date')) + b' ' + quoted(headers.get('Subject')) + b' NIL NIL NIL NIL NIL NIL NIL ' + quoted(headers.get('Message-ID')) + b')'
                            date = message['date']
                            response += f'* {sequence} FETCH (UID {uid} FLAGS ({flags}) RFC822.SIZE {len(message["raw"])} INTERNALDATE "{date}" ENVELOPE '.encode() + envelope + b')\r\n'
                        else:
                            response += f'* {sequence} FETCH (UID {uid} FLAGS ({flags}) RFC822.SIZE {len(message["raw"])})\r\n'.encode()
                elif operation == 'STORE':
                    flags_match = re.search(r'FLAGS(?:\.SILENT)?\s+\((.*?)\)', line, re.I)
                    flags = flags_match[1].split() if flags_match else []
                    for uid in selected_ids(arguments[1], messages):
                        messages[uid]['flags'] = sorted(set(messages[uid]['flags'] + flags)) if arguments[2].startswith('+') else flags
                        sequence = sorted(messages).index(uid) + 1
                        response += f'* {sequence} FETCH (UID {uid} FLAGS ({" ".join(messages[uid]["flags"])}))\r\n'.encode()
                elif operation == 'MOVE':
                    for uid in selected_ids(arguments[1], messages):
                        sequence = sorted(messages).index(uid) + 1
                        message = messages.pop(uid)
                        destination_uid = append(user, arguments[2], message['raw'], message['flags'])
                        response += f'* {sequence} EXPUNGE\r\n'.encode()
                    changed((user, folder))
                else:
                    writer.write(f'{tag} BAD unsupported UID command\r\n'.encode())
                    await writer.drain()
                    continue
            elif command not in ('NOOP', 'CHECK'):
                writer.write(f'{tag} BAD unsupported command\r\n'.encode())
                await writer.drain()
                continue
            writer.write(response + f'{tag} OK completed\r\n'.encode())
            await writer.drain()
    except (ConnectionError, asyncio.IncompleteReadError, ValueError, IndexError) as error:
        if not isinstance(error, (ConnectionError, asyncio.IncompleteReadError)):
            print(type(error).__name__ + ': ' + str(error), flush=True)
    finally:
        WATCH[user, folder].discard(writer)
        writer.close()
        try:
            await writer.wait_closed()
        except ConnectionError:
            pass


async def main():
    asyncio.sslproto.SSLProtocol.max_size = 4096
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain('/certs/server.crt', '/certs/server.key')
    ports = [int(port) for port in os.environ.get('SIMULATOR_PORTS', '993,1993,2993,3993,4993,5993,6993,7993').split(',')]
    servers = [await asyncio.start_server(connection, '0.0.0.0', port, ssl=context, limit=65536, ssl_handshake_timeout=30) for port in ports]
    print('TLS IMAP simulator listening on ' + ','.join(map(str, ports)), flush=True)
    await asyncio.gather(*(server.serve_forever() for server in servers))


if __name__ == '__main__':
    asyncio.run(main())
