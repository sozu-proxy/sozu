#!/bin/sh
openssl req -x509 -nodes -days 1825 -newkey rsa:2048 -keyout key.pem -out certificate.pem -config req.conf -extensions 'v3_req'

