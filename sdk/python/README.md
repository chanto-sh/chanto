# chanto-sdk

Python client for `chanto serve`, using only the standard library. [Install Chanto](https://chanto.sh/download) first.

```python
from chanto_sdk import Chanto

with Chanto.start() as h:
    s = h.session(mode="acceptEdits")
    print(s.run("add a --verbose flag to the CLI and update the tests"))
```

Experimental: the interface may change in any release. See the [SDK docs](https://chanto.sh/docs/sdk).
