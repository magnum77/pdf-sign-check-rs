sudo apt update
sudo apt install poppler-utils
sudo apt install libnss3-tools

mv ~/.pki/nssdb ~/.pki/nssdb.bak 2>/dev/null
mkdir -p ~/.pki/nssdb
chmod 700 ~/.pki/nssdb

certutil -d sql:$HOME/.pki/nssdb -N --empty-password
certutil -d sql:$HOME/.pki/nssdb -L
