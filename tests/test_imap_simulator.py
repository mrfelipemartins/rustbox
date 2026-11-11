import importlib.util
import pathlib
import unittest
import base64
from unittest.mock import patch

path = pathlib.Path(__file__).parents[1] / 'scripts' / 'imap_simulator.py'
spec = importlib.util.spec_from_file_location('simulator', path)
simulator = importlib.util.module_from_spec(spec)
spec.loader.exec_module(simulator)


class SimulatorTests(unittest.TestCase):
    def setUp(self):
        simulator.MAIL.clear()
        simulator.NEXT.clear()
        simulator.WATCH.clear()

    def test_uid_sequence_isolated_by_account_and_folder(self):
        raw = b'Message-ID: <one@test>\r\nSubject: Sample\r\n\r\nbody'
        self.assertEqual(simulator.append('alice', 'INBOX', raw), 1)
        self.assertEqual(simulator.append('alice', 'INBOX', raw), 2)
        self.assertEqual(simulator.append('bob', 'INBOX', raw), 1)
        self.assertEqual(simulator.append('alice', 'Sent', raw), 1)
        self.assertEqual(simulator.MAIL['alice']['INBOX'][1]['raw'], raw)
        self.assertEqual(simulator.MAIL['alice']['INBOX'][1]['headers']['Message-ID'], '<one@test>')

    def test_uid_ranges_include_imap_reverse_star_range(self):
        messages = {1: {}, 3: {}, 9: {}}
        self.assertEqual(simulator.selected_ids('1:3,9', messages), [1, 3, 9])
        self.assertEqual(simulator.selected_ids('10:*', messages), [9])
        self.assertEqual(simulator.selected_ids('1:*', {}), [])

    def test_envelope_quoting(self):
        self.assertEqual(simulator.quoted(None), b'NIL')
        self.assertEqual(simulator.quoted('a"b\r\nc'), b'"a\\"bc"')


class OAuthSimulatorTests(unittest.IsolatedAsyncioTestCase):
    async def test_synchronized_expiry_and_reconnect(self):
        server = await simulator.asyncio.start_server(simulator.connection, '127.0.0.1', 0)
        port = server.sockets[0].getsockname()[1]

        async def login(index, epoch, accepted):
            reader, writer = await simulator.asyncio.open_connection('127.0.0.1', port)
            await reader.readline()
            writer.write(b'A AUTHENTICATE XOAUTH2\r\n')
            await writer.drain()
            self.assertEqual(await reader.readline(), b'+ \r\n')
            token = base64.b64encode(f'user=user{index}@test\x01auth=Bearer simulator:{epoch}\x01\x01'.encode())
            writer.write(token + b'\r\n')
            await writer.drain()
            reply = await reader.readline()
            if not accepted:
                self.assertTrue(reply.startswith(b'+ '))
                writer.write(b'\r\n')
                await writer.drain()
                reply = await reader.readline()
            self.assertTrue(reply.startswith(b'A OK' if accepted else b'A NO'))
            writer.close()
            await writer.wait_closed()

        try:
            with patch.dict(simulator.os.environ, {'SIMULATOR_OAUTH_TOKEN_SECONDS': '60'}):
                with patch.object(simulator.time, 'time', return_value=600):
                    await simulator.asyncio.gather(*(login(i, 10, True) for i in range(100)))
                with patch.object(simulator.time, 'time', return_value=660):
                    await simulator.asyncio.gather(*(login(i, 10, False) for i in range(100)))
                    await simulator.asyncio.gather(*(login(i, 11, True) for i in range(100)))
        finally:
            server.close()
            await server.wait_closed()


if __name__ == '__main__':
    unittest.main()
