# How are Sōzu's workers managed?

Sōzu's main process starts and manages _workers_, which are subinstances of itself.
This core feature makes Sōzu pretty efficient, but raises the question of managing state accross a whole cluster of processes.

How do we solve this challenge? Unix sockets and channels.

## Architecture

`sozuctl` sends commands on a unix socket.
In the `command::start_server()` function of the main process,
there is a thread running in the background where a unix listener accepts new
connection and spawns client loops.

The client loops parse client requests and forward them to the Command Server
through mpsc channels. **mpsc** = Multiple Producer, Single Consumer.
The sending end of the channel, called `command_tx`, is cloned and used many times over,
but the messages are all received by a single `command_rx` in the Command Server main loop.

```
        UNIX                   UNIX
       SOCKET                 SOCKET
        | ^                    | ^
        | |                    | |
   +----v-+-----+         +----v-+-----+
   |   client   |         |   client   |       as many more
   |    loop    |         |    loop    |       clients as we
   +-+-------^--+         +--+-----^---+          want
     |       |               |     |
     |       | mpsc channels |     |
     |       |               |     |
+----v-------+---------------v-----+------+
|                                         |
|                 Command                 |
|                  Server                 |
|                                         |
+----+-------^---------------+-----^------+
     |       |               |     |
     |       | mpsc channels |     |
     |       |               |     |
   +-v-------+--+         +--v-----+---+
   |   worker   |         |   worker   |     as many more
   |    loop    |         |    loop    |     workers as we
   +----+-^-----+         +----+-^-----+        want
        | |                    | |
        v |                    v |
        UNIX                   UNIX
       SOCKET                 SOCKET
```

As you can guess from the drawing, the exact same logic applies when workers send messages
to the CommandServer.

The Command Server is able to send messages to clients and to workers by
keeping track of the sending ends of their mpsc channels, `client_tx` and `worker_tx`.

In turn, clients and workers listen on their own receivers, `client_rx` and `worker_rx`, and
write everything onto their respective unix streams, to notify clients and workers.

# Asynchronous handling of commands

It is impossible to manage commands synchronously.
Some tasks are fast enough (for example, dumping the state), some are way too long.
For instance, loading a new state implies to:

-   parse a state file to derive instructions from it
-   send ALL instructions to ALL workers
-   wait for ALL workers to reply

Blocking the main thread is unthinkable. Therefore, Sōzu detaches threads by doing, for instance:

```rust
smol::spawn(
    client_loop(id, unix_stream, command_tx, client_rx)
).detach();
```

This make the client loop run in the background.
Using similar syntax, we can wait for worker responses in the background.
But how can we bring data back from those threads? => **more channels**.

# The flow of requests, responses, and detached threads.

What the Command Server does to perform a task:

```
                               +-------------+
                               |             |
                               |   client    |
                               |    loop     |
                               |             |
                               +----+--------+
                                    |                MAIN THREAD                       DETACHED THREAD
                         +----------+-------------------------------------------+    +------------------+
                         |          |                                           |    |                  |
                         |          |            create mpsc channel            |    |                  |
   +--------------+      |          |            +------------------+           |    |                  |
   |              |      |          v            v                  v           |    |   Listen on      |
+--+ worker loop  |<--+--+------ REQUEST     SENDER              RECEIVER       |    |   the receiver   |
|  |              |   |  |            |        |                    |           |    |                  |
|  +--------------+   |  |            |        |                    |           |    |                  |
|                     |  |            v        |                    |           |    |   Wait for all   |
|  +--------------+   |  |           id        |                    |           |    |   responses      |
|  |              |   |  |            |        |                    +-----------+--->|                  |
+--+ worker loop  |<--+  |            |        |                                |    |                  |
|  |              |   |  |            |        |                                |    |   apply logic    |
|  +--------------+   |  |            v        v                                |    |                  |
|                     |  |        in_flight hash map     use          RESPONSES |    |                  |
|  +--------------+   |  |        +-----------------+   sender    +-------------+--->|                  |
|  |              |   |  |        |                 +------------>|             |    |                  |
+--+ worker loop  |<--+  |        |  -request ids   |             |             |    |                  |
|  |              |      |        |                 |  retrieve   |             |    |   Send final     |
|  +--------------+      |        |  -senders       |   sender    |             |    |   result to the  |
|                        |        |                 |<-----+      |             |    |   main thread    |
|                        |        +-----------------+      |      |             |    |   (not shown)    |
|                        |                                id      |             |    |                  |
|                        |                                 ^      |             |    |                  |
|                        |                                 |      |             |    |                  |
+------------------------+->RESPONSES----------------------+------+             |    |                  |
                         |                                                      |    |                  |
                         |                                                      |    |                  |
                         +------------------------------------------------------+    +------------------+
```

## What the main thread does to client requests

-   **Receive a client request** through the client loop, and if this request necessitates to talk to the workers,
-   **send requests to the workers** through the worker loop. This goes fast.
-   **create an mpsc** task channel with two ends, the _sender_ and the _receiver_.
-   in a hash map called `in_flight`, keep track of:
    -   the `request_id`
    -   the _sender_
-   Give the _receiver_ to a **detached thread**

## What the main thread does to worker responses

-   **Receives worker responses** through the worker loop
-   **Looks at the `response_id`**, which is the same as the `request_id` seen above
-   searches the `in_flight` hash map to **retrieve the associated _sender_**
-   uses the _sender_ to **send the response into the detached thread**

## What the detached thread does

-   **waits for worker responses** on the _receiver_
-   Completes the logic
-   sends the final response **back to the command server** using `command_tx`,
    _just like client loops and worker loops do_, because they are detached threads too.

The Command Server just puts this final response into the client loop, and _voilà_!

## To sum up

Here is what is delegated into the background (all those boxes around the main thread):

1. reading and writing from/onto the unix sockets
2. waiting for and processing worker responses

The Command Server can be described as event-based because everything is returned
to the main loop using channels, in no precise order, asynchronously.
