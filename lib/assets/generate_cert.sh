#!/bin/sh
openssl req -x509 -nodes -days 1825 -newkey rsa:2048 -keyout local-key.pem -out local-certificate.pem -config local.conf