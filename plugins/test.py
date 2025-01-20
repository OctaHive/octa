#!/usr/bin/env python3
import sys
import json
import asyncio
import os
import platform
from datetime import datetime

def log_message(log_file, message):
    timestamp = datetime.now().strftime("%Y-%m-%d %H:%M:%S.%f")
    with open(log_file, 'a') as f:
        f.write(f"[{timestamp}] {message}\n")

class CommandHandler:
    def __init__(self, log_file):
        self.log_file = log_file

    def handle_hello(self, cmd):
        response = {
            "type": "Hello",
            "version": "0.1.0",
            "features": []
        }
        return False, [response]

    def handle_execute(self, cmd):
        response = []

        # Send Started
        started = {
            "type": "Started",
            "id": "test-execution-id"
        }
        response.append(started)

        # Send stdout
        stdout = {
            "type": "Stdout",
            "id": "test-execution-id",
            "line": "test output"
        }
        response.append(stdout)

        # Send exit status
        exit_status = {
            "type": "ExitStatus",
            "id": "test-execution-id",
            "code": 0
        }
        response.append(exit_status)

        return False, response

    def handle_schema(self, cmd):
      response = {
          "type": "Schema",
          "key": "key",
      }
      return False, [response]

    def handle_shutdown(self, cmd):
        response = {
            "type": "Shutdown",
            "message": "Shutting down"
        }
        return True, [response]

    def handle_command(self, cmd):
        handlers = {
            "Hello": self.handle_hello,
            "Schema": self.handle_schema,
            "Execute": self.handle_execute,
            "Shutdown": self.handle_shutdown
        }

        cmd_type = cmd.get("type")
        if cmd_type in handlers:
            return handlers[cmd_type](cmd)
        else:
            log_message(self.log_file, f"Unknown command type: {cmd_type}")
            return False, []

class WindowsServer:
    def __init__(self, path, log_file):
        self.path = fr"\\.\pipe\{path}"
        self.log_file = log_file
        self.pipe = None

    async def handle_client(self, pipe):
        import win32pipe
        import win32file

        loop = asyncio.get_running_loop()
        command_handler = CommandHandler(self.log_file)

        while True:
            data = await loop.run_in_executor(None, win32file.ReadFile, pipe, 4096)
            message = data[1].decode()
            log_message(self.log_file, f"Received: {message}")

            try:
                cmd = json.loads(message)
                should_close, response = command_handler.handle_command(cmd)

                for message in response:
                    response_json = json.dumps(message) + "\n"
                    log_message(self.log_file, f"Sending: {json.dumps(message)}")
                    await loop.run_in_executor(None, win32file.WriteFile, pipe, response_json.encode())

                if should_close:
                    break
            except json.JSONDecodeError as e:
                log_message(self.log_file, f"Invalid JSON received: {e}")

        print("Closing pipe...")
        win32file.CloseHandle(pipe)

    async def start(self):
        import win32pipe
        import win32file

        log_message(self.log_file, f"Starting Windows named pipe server on: {self.path}")

        self.pipe = win32pipe.CreateNamedPipe(
            self.path,
            win32pipe.PIPE_ACCESS_DUPLEX,
            win32pipe.PIPE_TYPE_MESSAGE | win32pipe.PIPE_READMODE_MESSAGE | win32pipe.PIPE_WAIT,
            1,
            65536,
            65536,
            0,
            None
        )

        try:
            await asyncio.get_running_loop().run_in_executor(None, win32pipe.ConnectNamedPipe, self.pipe, None)
            print("Client connected")
        except Exception as e:
            win32file.CloseHandle(self.pipe)
            log_message(self.log_file, f"Error connect pipe: {str(e)}")
            raise

    async def serve_forever(self):
      await asyncio.create_task(self.handle_client(self.pipe))

    async def close(self):
        import win32file

        if self.pipe:
            win32file.CloseHandle(self.pipe)

class UnixServer:
    def __init__(self, path, log_file):
        self.path = path
        self.log_file = log_file
        self.server = None

    async def handle_client(self, reader, writer, log_file):
      command_handler = CommandHandler(log_file)

      try:
          while True:
              line = await reader.readline()
              if not line:
                  break

              received = line.decode().strip()
              log_message(log_file, f"Received: {received}")

              try:
                  cmd = json.loads(received)
                  should_close, response = command_handler.handle_command(cmd)

                  for message in response:
                      response_json = json.dumps(message) + "\n"
                      log_message(self.log_file, f"Sending: {json.dumps(message)}")
                      writer.write(response_json.encode())
                      await writer.drain()

                  if should_close:
                      break
              except json.JSONDecodeError as e:
                  log_message(log_file, f"Invalid JSON received: {e}")
              except Exception as e:
                  log_message(log_file, f"Error handling command: {e}")

      finally:
          writer.close()
          await writer.wait_closed()

    async def start(self):
        log_message(self.log_file, f"Starting Unix domain socket server on: {self.path}")

        try:
            os.unlink(self.path)
        except FileNotFoundError:
            pass

        self.server = await asyncio.start_unix_server(
            lambda r, w: self.handle_client(r, w, self.log_file),
            self.path
        )

    async def serve_forever(self):
        async with self.server:
            await self.server.serve_forever()

    async def close(self):
        if self.server:
            self.server.close()
            await self.server.wait_closed()
            try:
                os.unlink(self.path)
            except FileNotFoundError:
                pass

def create_server(path, log_file):
    if platform.system() == "Windows":
        return WindowsServer(path, log_file)
    else:
        return UnixServer(path, log_file)

async def main():
    socket_path = None
    for i, arg in enumerate(sys.argv):
        if arg == "--socket-path" and i + 1 < len(sys.argv):
            socket_path = sys.argv[i + 1]
            break

    if not socket_path:
        print("Socket path not provided", file=sys.stderr)
        sys.exit(1)

    log_file = "test_plugin.log"
    server = create_server(socket_path, log_file)

    try:
        await server.start()
        log_message(log_file, "Server started successfully")
        await server.serve_forever()
    except asyncio.CancelledError:
        log_message(log_file, "Server was cancelled")
    except Exception as e:
        log_message(log_file, f"Server error: {e}")
    finally:
        log_message(log_file, "Shutting down server...")
        await server.close()
        log_message(log_file, "Server shut down complete")

if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        print("Server stopped by user")
