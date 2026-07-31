#!/usr/bin/env python3
"""Round-trip test: stand-in for a bank's CBS (A2 webhook_obp receiver).

Accepts POST /credit-notifications with Bearer rt-local-secret, appends each
body to data/cbs_received.jsonl, replies {"status": "...", "cbs_reference"}.
"""
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "data", "cbs_received.jsonl")
SECRET = "rt-local-secret"
counter = 0


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        global counter
        if self.path != "/credit-notifications":
            self.send_response(404)
            self.end_headers()
            return
        if self.headers.get("Authorization") != f"Bearer {SECRET}":
            self.send_response(401)
            self.end_headers()
            return
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length).decode("utf-8", "replace")
        counter += 1
        ref = f"RT-CBS-{counter:04d}"
        os.makedirs(os.path.dirname(OUT), exist_ok=True)
        with open(OUT, "a") as f:
            f.write(json.dumps({"cbs_reference": ref, "body": json.loads(body)}) + "\n")
        print(f"CBS stub: credited {ref}: {body[:200]}", flush=True)
        resp = json.dumps({"status": "BOOKED", "cbs_reference": ref}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(resp)))
        self.end_headers()
        self.wfile.write(resp)

    def log_message(self, *a):
        pass


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 9009
    print(f"CBS stub listening on :{port}, appending to {OUT}", flush=True)
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()
