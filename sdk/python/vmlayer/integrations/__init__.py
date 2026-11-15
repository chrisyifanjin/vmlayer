"""Adapters that plug vmlayer sandboxes into agent frameworks.

Each module needs its framework installed and imports it lazily, so
``import vmlayer`` never pulls one in:

* ``vmlayer.integrations.minisweagent`` — ``Environment`` for mini-swe-agent
  (``pip install "vmlayer[minisweagent]"``)
* ``vmlayer.integrations.autogen`` — ``CodeExecutor`` for AutoGen
  (``pip install "vmlayer[autogen]"``)
"""
