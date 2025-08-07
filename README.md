## Online Division N.A.T. Piercer

### Short Description

The O.D. N.A.T. Piercer Protocol is a special case of the U.D.P Hole-Punching N.A.T. Traversal Technique that is used for secured, low sensitivity message transport. Such use cases include Voice-over-I.P. communication, Video-over-I.P. type communication, and Screen-Sharing communication.

### Installation and usage

Clone this repository:

```
$ git clone 'https://github.com/onlinedi-vision/od-nat-piercer.git'
``` 

And build the project using:

- cargo:
```
$ cargo build --release
```
- docker:
```
# This is NOT yet available
$ docker build -t nat_piercer .
```

Then run it:

- cargo build:
```
$ ./target/release/od_nat_piercer
```
- docker run:
```
$ docker run -d -p 1001:1001 nat_piercer:latest
```
