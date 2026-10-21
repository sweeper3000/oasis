#!/usr/bin/env python3
"""Explicit FTPS fixture for test_external_storage.py (test-only credentials)."""
import sys
from OpenSSL import SSL
from pyftpdlib.authorizers import DummyAuthorizer
from pyftpdlib.handlers import TLS_FTPHandler
from pyftpdlib.servers import FTPServer

root, port, cert, key = sys.argv[1:]
authorizer = DummyAuthorizer()
authorizer.add_user("tester", "remote-test-password", root, perm="elradfmwMT")
# Keep the fixture on TLS 1.2; some pyOpenSSL/Python combinations truncate
# buffered data when receiving a TLS 1.3 close-notify on loopback.
TLS_FTPHandler.ssl_options |= SSL.OP_NO_TLSv1_3
TLS_FTPHandler.authorizer = authorizer
TLS_FTPHandler.certfile = cert
TLS_FTPHandler.keyfile = key
TLS_FTPHandler.tls_control_required = True
TLS_FTPHandler.tls_data_required = True
TLS_FTPHandler.masquerade_address = "127.0.0.1"
FTPServer(("127.0.0.1", int(port)), TLS_FTPHandler).serve_forever()
