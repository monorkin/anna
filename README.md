# ANNA

> Jag känner en bot, hon heter Anna, Anna heter hon
> -- Basshunter, Botten Anna

Anna is a self-governing, token-efficent, agent and harness that you can
work with through Basecamp, HEY & Fizzy like she was your human colege.

She uses your regular Claude Code subscription, runs everything in
sandboxes, can auto-switch between multiple Claude subscriptions,
and learns and improves over time while also being token efficient.

> ![WARNING]
> Anna is experimental. This is just an idea I'm playing around with.
> Consider all of this a giant Rube Goldberg machine that's actively
> being tweaked.

## Setting her up

Before you start, you'll have to have Claude Code installed and at least 
one active subscription for it.

The easiest way to install Anna is through mise. Just run the following:

```bash
# TODO
```

Once installed, start the setup process. It will guide you through everything:

```bash
anna setup
```

## Usage

```bash
# Running
anna run # starts anna

# Subscription management
anna claude account add # add the current Claude Code account to the rotation
anna claude account remove email@example.com
anna claude account list

# Memory
anna memory list
anna memory archive [memory-id]

# Logs
anna log [-f] [--tail]
```

## Architecture

Anna is built around the brain and hands principle. In short, this means
that there is a "brain" that accomplishes tasks using its "hands".

Anna's brain consists of a dispatcher and multiple threads, 
each with its own reviewer.

Each hand is a sandboxed instance of Claude Code that can only talk to its brain. It has
very limited disk access, and no internet access besides localhost. All actions it wants
to take, that go beyond the sandbox go through an MPC request that's arbitrated by a
broker.

```
+- SOURCE -----------------------------------------------------------+
|                                                                    |
|            Basecamp / Fizzy / HEY / Github / Sentry                |
|                                                                    |
+----^----------------------------+-----------------------------^----+
     |                            |                             |
     | replies,                   | messages,          replies, |
     | comments                   | assignments        comments |
     |                            |                             |
+----|-- BRAIN -------------------|-----------------------------|----+
|    |                            |                             |    |
|    |               +------------v-------------+               |    |
|    |               |        DISPATCHER        |               |    |
|    |               |  classifies and routes   |               |    |
|    |               +-----+--------------+-----+               |    |
|    |                     |              |                     |    |
|    |             message |              | message             |    |
|    |                     |              |                     |    |
|  +-+------- THREAD ------v----+    +----v---- THREAD ---------+-+  |
|  |  SESSION       REVIEWER    |    |  SESSION       REVIEWER    |  |
|  |  plans, writes checks the  |    |                            |  |
|  |  brief + grant hand's work |    |                            |  |
|  +---+------------^-----------+    +-----+------^------+------^-+  |
|      | brief +    | result +             |      |      |      |    |
|      | grant      | transcript           |      |      |      |    |
|      |            |                      |      |      |      |    |
|  +---v------------+----------------------v------+------v------+-+  |
|  |  BROKER   enforces each hand's grant, carries MCP actions    |  |
|  |           and Claude API traffic                             |  |
|  +---+------------^---------------+------+------^------+------^-+  |
|      |            |               |      |      |      |      |    |
|      |            |  +- MEMORY ---v---+  |      |      |      |    |
|      |            |  | transcript >   |  |      |      |      |    |
|      |            |  | extract > rules|  |      |      |      |    |
|      |            |  | > risk > store |  |      |      |      |    |
|      |            |  +----------------+  |      |      |      |    |
|      |            |                      |      |      |      |    |
+------+------------+----------------------+------+------+------+----+
       |            |                      |      |      |      |
       |            |                      |      |      |      |
   +---v--- HAND ---+-----------+        +-v HAND +-+  +-v HAND +-+
   | Claude Code in a sandbox:  |        |          |  |          |
   | one project folder, no     |        |          |  |          |
   | network, deleted when the  |        |          |  |          |
   | thread is done with it     |        |          |  |          |
   +----------------------------+        +----------+  +----------+
```
