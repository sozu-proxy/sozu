#!/bin/bash


echo "127.0.0.1 lolcatho.st" >> /etc/hosts

echo "" > /etc/nginx/sites-enabled/mytest
echo "server {" >> /etc/nginx/sites-enabled/mytest
echo "        listen 1026 default_server;" >> /etc/nginx/sites-enabled/mytest
echo "        root /var/www/html;" >> /etc/nginx/sites-enabled/mytest
echo "        index index.html index.htm index.nginx-debian.html;" >> /etc/nginx/sites-enabled/mytest
echo "        server_name lolcatho.st;" >> /etc/nginx/sites-enabled/mytest
echo "        location / {" >> /etc/nginx/sites-enabled/mytest
echo "            try_files $uri $uri/ =404;" >> /etc/nginx/sites-enabled/mytest
echo "        }" >> /etc/nginx/sites-enabled/mytest
echo "}" >> /etc/nginx/sites-enabled/mytest

rm /etc/nginx/sites-enabled/default

service nginx start
