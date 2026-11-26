"""Exercise the Contract v1 wire format with an isolated durable demo provider."""
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import sys
from tempfile import TemporaryDirectory
import time
import unittest
from urllib import error, request
from uuid import uuid4

HERE = Path(__file__).resolve().parent

class ContractTest(unittest.TestCase):
    def setUp(self):
        self.temp = TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.token = secrets.token_hex(32)
        self.child = None
        self.addCleanup(self.stop)

    def start(self, drop=False):
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        self.origin = f"http://127.0.0.1:{port}"
        config_file = Path(self.temp.name) / "config.json"
        config_file.write_text(json.dumps({"drop_response":drop}), encoding="utf-8")
        self.child = subprocess.Popen([sys.executable, str(HERE / "demo.py"),
            "--port",str(port), "--config-file",str(config_file),"--data-dir",self.temp.name],
            env={**os.environ, "VF_PLUGIN_TOKEN":self.token, "VF_PLUGIN_SECRETS_JSON":"{}"},
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(100):
            if self.child.poll() is not None:
                self.fail("fixture exited during startup")
            try:
                if self.call("/healthz")[0] == 200:
                    return
            except OSError:
                time.sleep(0.05)
        self.fail("fixture not ready")

    def stop(self):
        if self.child and self.child.poll() is None:
            self.child.terminate()
            self.child.wait(timeout=10)

    def call(self,path,method="GET",body=None,key=None,token=None):
        headers={"Authorization":"Bearer "+(token or self.token),"Content-Type":"application/json"}
        if key:
            headers["Idempotency-Key"]=key
        req=request.Request(self.origin+path,method=method,data=json.dumps(body).encode() if body is not None else None,headers=headers)
        try:
            reply=request.urlopen(req, timeout=3)
        except error.HTTPError as exc:
            reply=exc
        with reply:
            return reply.status,json.load(reply)

    def test_lost_response_restarts_and_cancel_race(self):
        self.start(drop=True)
        self.assertEqual(self.call("/healthz", token="wrong")[0],401)
        key=str(uuid4())
        image=json.loads((HERE/"responses.json").read_text("utf-8"))["image_lookup_200"]["data"][0]["b64_json"]
        payload={"model":"m1","prompt":"one","image":"data:image/png;base64,"+image,"size":"1280x720","seconds":8}
        with self.assertRaises(ConnectionError):
            self.call("/v1/videos","POST",payload,key)
        self.stop()
        self.start()
        status,op=self.call("/vf-plugin/v1/operations/"+key)
        self.assertEqual((status,op["status"],op["task_id"]),(200,"succeeded",key))
        self.assertEqual(self.call("/v1/videos","POST",payload,key)[1]["task_id"],key)
        self.assertEqual(self.call("/v1/videos","POST",{**payload,"seconds":4},key)[0],409)
        late=str(uuid4())
        self.assertEqual(self.call("/vf-plugin/v1/operations/"+late,"DELETE")[1]["status"],"cancelled")
        self.stop()
        self.start()
        self.assertEqual(self.call("/v1/videos","POST",payload,late)[0],409)

if __name__=="__main__":
    unittest.main()
