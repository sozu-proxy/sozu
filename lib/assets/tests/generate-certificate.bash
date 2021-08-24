#!/usr/host/bin/bash

function generate-certificate() {
	openssl req -new -out certificate-$1.csr -key key.pem -config tls.conf
	openssl x509 -req -days 730 -in certificate-$1.csr -signkey key.pem -out certificate-$1.pem -extensions req_ext -extfile tls.conf
}
