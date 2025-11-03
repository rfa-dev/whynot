# whynot

https://wainao.me/ backup service.

## Usage

Option 1: Build from Source

```bash
## Install rust with the official guide
https://www.rust-lang.org/tools/install

## Clone the repository
git clone https://github.com/rfa-dev/whynot

## The compiled executable will be located at target/release/
cargo build -r
```

Option 2: Download Prebuilt Binary

👉 https://github.com/rfa-dev/rfa/releases

### Crawling website

`./target/release/spider` or `./spider`

More options:

```bash
whynot website crawler, downloading lists, pages and imgs

Usage: spider [OPTIONS]

Options:
      --proxy <PROXY>    proxy (e.g., http://127.0.0.1:8089)
  -o, --output <OUTPUT>  [default: whynot_data]
  -h, --help             Print help
```

### Online service

`./target/release/web` or `./web`

More options:

```bash
WHYNOT backup website

Usage: web [OPTIONS]

Options:
  -a, --addr <ADDR>  listening address [default: 127.0.0.1:3334]
  -d, --data <DATA>  data folder, containing imgs/ and whynot.db/ [default: whynot_data]
  -h, --help         Print help
```
