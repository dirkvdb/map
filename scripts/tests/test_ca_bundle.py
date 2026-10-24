#!/usr/bin/env python3
"""Offline native TLS regressions; run inside devenv with MAP_RUN_NATIVE_TESTS=1."""

import http.server
import os
import shlex
import ssl
import subprocess
import tempfile
import threading
import unittest
from pathlib import Path

HARNESS = r'''
#include <curl/curl.h>
#include <cstdio>

static size_t discard(char *, size_t size, size_t count, void *) {
    return size * count;
}

int main(int argc, char **argv) {
    if (argc != 2 || curl_global_init(CURL_GLOBAL_DEFAULT) != CURLE_OK) return 1;
    CURL *handle = curl_easy_init();
    if (!handle) return 1;
    CURLcode result = CURLE_OK;
    for (int request = 0; request < 2 && result == CURLE_OK; ++request) {
        // MapLibre resets pooled handles between requests.
        curl_easy_reset(handle);

        char error[CURL_ERROR_SIZE] = {};
        curl_easy_setopt(handle, CURLOPT_ERRORBUFFER, error);
        curl_easy_setopt(handle, CURLOPT_URL, argv[1]);
        curl_easy_setopt(handle, CURLOPT_WRITEFUNCTION, discard);
        curl_easy_setopt(handle, CURLOPT_NOPROXY, "*");
        curl_easy_setopt(handle, CURLOPT_FRESH_CONNECT, 1L);
        curl_easy_setopt(handle, CURLOPT_TIMEOUT, 5L);
        result = curl_easy_perform(handle);
        if (result != CURLE_OK) std::fprintf(stderr, "%s\n", error);
    }
    curl_easy_cleanup(handle);
    curl_global_cleanup();
    return static_cast<int>(result);
}
'''


class Handler(http.server.BaseHTTPRequestHandler):
    def handle(self):
        try:
            super().handle()
        except (ConnectionError, ssl.SSLError):
            # Rejected certificates can close the TLS connection before an HTTP request.
            pass

    def do_GET(self):
        body = b"verified\n"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args):
        pass


@unittest.skipUnless(
    os.environ.get("MAP_RUN_NATIVE_TESTS") == "1",
    "set MAP_RUN_NATIVE_TESTS=1 inside devenv to compile and exercise native TLS",
)
class CABundleTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        temporary = tempfile.TemporaryDirectory(prefix="map TLS test ")
        cls.addClassCleanup(temporary.cleanup)
        cls.root = Path(temporary.name)

        source = cls.root / "request.cpp"
        source.write_text(HARNESS)
        cls.binary = cls.root / "request"
        flags = subprocess.check_output(
            ["pkg-config", "--cflags", "--static", "--libs", "libcurl"], text=True,
        )
        subprocess.run([
            *shlex.split(os.environ.get("CXX", "c++")), "-std=c++17",
            "-Wall", "-Wextra", "-Werror",
            str(source), "-o", str(cls.binary), *shlex.split(flags),
        ], check=True, text=True)

        for name in ("server", "unrelated"):
            subprocess.run([
                "openssl", "req", "-x509", "-newkey", "rsa:2048", "-noenc",
                "-days", "1", "-batch", "-config", "/dev/null",
                "-subj", "/CN=Map TLS test", "-addext", "subjectAltName=IP:127.0.0.1",
                "-addext", "basicConstraints=critical,CA:TRUE",
                "-keyout", str(cls.root / f"{name}.key"),
                "-out", str(cls.root / f"{name}.pem"),
            ], check=True, capture_output=True, text=True)

        cls.empty_ca_directory = cls.root / "empty-ca"
        cls.empty_ca_directory.mkdir()
        (cls.root / "invalid.pem").write_text("not a certificate\n")
        cls.ca_directory = cls.root / "hashed-ca"
        cls.ca_directory.mkdir()
        (cls.ca_directory / "server.pem").write_bytes((cls.root / "server.pem").read_bytes())
        subprocess.run(
            ["openssl", "rehash", str(cls.ca_directory)],
            check=True, capture_output=True, text=True,
        )

        cls.server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        cls.addClassCleanup(cls.server.server_close)
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(cls.root / "server.pem", cls.root / "server.key")
        cls.server.socket = context.wrap_socket(cls.server.socket, server_side=True)
        cls.port = cls.server.server_port
        thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        thread.start()
        cls.addClassCleanup(thread.join, 5)
        cls.addClassCleanup(cls.server.shutdown)

    def request(self, variables, host="127.0.0.1"):
        env = dict(os.environ)
        for name in ("CURL_CA_BUNDLE", "SSL_CERT_FILE", "SSL_CERT_DIR"):
            env.pop(name, None)
        # Prevent OpenSSL's default CA directory from masking invalid file overrides.
        env["SSL_CERT_DIR"] = str(self.empty_ca_directory)
        env.update(variables)
        return subprocess.run(
            [str(self.binary), f"https://{host}:{self.port}/"],
            env=env, capture_output=True, text=True, timeout=15,
        )

    def test_ssl_cert_file_trusts_local_ca_after_handle_reset(self):
        result = self.request({"SSL_CERT_FILE": str(self.root / "server.pem")})
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_invalid_ssl_cert_file_fails_verification(self):
        result = self.request({"SSL_CERT_FILE": str(self.root / "invalid.pem")})
        # OpenSSL fallback fails peer verification, not explicit CAINFO loading (77).
        self.assertEqual(result.returncode, 60, result.stderr)

    def test_missing_ssl_cert_file_fails_verification(self):
        result = self.request({"SSL_CERT_FILE": str(self.root / "missing.pem")})
        self.assertEqual(result.returncode, 60, result.stderr)

    def test_unrelated_ca_does_not_trust_server(self):
        result = self.request({"SSL_CERT_FILE": str(self.root / "unrelated.pem")})
        self.assertEqual(result.returncode, 60, result.stderr)

    def test_hashed_ssl_cert_dir_trusts_local_ca_after_handle_reset(self):
        result = self.request({
            "SSL_CERT_FILE": str(self.root / "missing.pem"),
            "SSL_CERT_DIR": str(self.ca_directory),
        })
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_curl_ca_bundle_alone_is_ignored_by_libcurl(self):
        result = self.request({"CURL_CA_BUNDLE": str(self.root / "server.pem")})
        self.assertEqual(result.returncode, 60, result.stderr)

    def test_curl_ca_bundle_cannot_override_ssl_cert_file(self):
        result = self.request({
            "CURL_CA_BUNDLE": str(self.root / "server.pem"),
            "SSL_CERT_FILE": str(self.root / "unrelated.pem"),
        })
        self.assertEqual(result.returncode, 60, result.stderr)

    def test_missing_curl_ca_bundle_does_not_prevent_ssl_cert_file_trust(self):
        result = self.request({
            "CURL_CA_BUNDLE": str(self.root / "missing.pem"),
            "SSL_CERT_FILE": str(self.root / "server.pem"),
        })
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_unset_overrides_preserve_default_certificate_verification(self):
        result = self.request({})
        self.assertEqual(result.returncode, 60, result.stderr)

    def test_hostname_verification_remains_enabled(self):
        result = self.request({"SSL_CERT_FILE": str(self.root / "server.pem")}, host="localhost")
        self.assertEqual(result.returncode, 60, result.stderr)


if __name__ == "__main__":
    unittest.main()
