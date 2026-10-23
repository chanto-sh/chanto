# Terminal control sequences in model output. One per prompt.
TURNS = [
    {"text": "plain answer first"},
    {"text": "clear coming:\u001b[2J\u001b[3J\u001b[H after clear"},
    {"text": "title:\u001b]0;PWNED-TITLE\u0007 osc52:\u001b]52;c;cHduZWQ=\u0007 link:\u001b]8;;http://evil\u0007click\u001b]8;;\u0007 done"},
    {"text": "c1 csi:\u009b2J after c1"},
]
