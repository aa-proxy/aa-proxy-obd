# 🚗 canze-rs

## How it works
![Hardware overview](images/aa-proxy-obd.webp)

## Description
This small Linux tool is intented to connect to a Renault Zoe's CAN bus over a Bluetooth ELM327 OBD-II adapter and forwards live battery telemetry to a local REST endpoint, designed to integrate with [`aa-proxy-rs`](https://github.com/manio/aa-proxy-rs).<br>
The name of the project is inspired by (and a tribute to) the great [CanZE](https://canze.fisch.lu/) project.<br>

#### This tool polls and transmits the following parameter:
- SOC (state of charge)

It is intended to run continuously as a daemon, sensing when the car's OBD dongle is in range.
When the car is awake (e.g. while charging or driving), it polls the parameter over ELM327 and `POST`s the reading as JSON to `http://localhost/battery` for `aa-proxy-rs` to consume.

## Usage
```
canze-rs 0.1.0
Renault Zoe state-of-charge telemetry logger

USAGE:
    canze-rs [OPTIONS]

OPTIONS:
    -c, --config <CONFIG>    Config file path [default: /etc/canze-rs.conf]
    -d, --debug              Enable debug info
    -h, --help               Print help information
    -V, --version            Print version information
```

## Config
The project uses a simple configuration file:<br>
`/etc/canze-rs.conf`<br>

The configuration file should have the following contents:<br>
```
[general]
mac = 00:00:00:00:00:00  # enter your Bluetooth OBD-II dongle MAC here
```
