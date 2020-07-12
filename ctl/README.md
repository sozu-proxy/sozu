# sozuctl, a command line interface for the Sōzu proxy

sozuctl provides various commands like shutdown, upgrade, status etc.

There are three options:

1. config  - Sets a custom config file
2. timeout - "Sets a custom timeout for commands (in milliseconds). 0 disables the timeout"
3. cmd     - Various subcommands


The cmd provides subcommands such as:

- shutdown
- upgrade
- status
- metrics
- application
- frontend
- certificate
- query 
- config

If you would like to know more about sozuctl you can get complete list by running --help.

```bash
$ sozuctl --help
```

### Exploring the source code.
`main.rs`  uses App struct which is used to initilize Commandline Interface Application. It uses
structopt crate for that.  `command.rs` defines command_timeout! macro which is used to send 
messages using mpsc queue. And as it name says we can define timeout in millisecond hence waits 
for command to be completed within given timeout.

`rand` crate is used to generate random number as seen in `command.rs` functions `generate_id` and `generate_tagged_id`.

`prettytable-rs` is used to display beautiful table in CLI App.