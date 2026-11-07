"""The phone's offline agent prompt and tools.

Keep in sync with mobile/src/lib/localAgent.ts (TOOLS and systemPrompt):
the on-device model is trained on exactly what the phone sends it.
"""

# localAgent.ts completion temperature.
TEMPERATURE = 0.3

TOOLS = [
    {
        "name": "set_reminder",
        "description": "Remind the user about something at a time.",
        "parameters": {
            "type": "object",
            "properties": {
                "what": {"type": "string", "description": "What to remind them about, short, in their words."},
                "when": {"type": "string", "description": 'When, in plain words: "tomorrow 9am", "at 5pm", "tonight".'},
                "in_minutes": {"type": "integer", "description": "Minutes from now, if they gave a duration."},
            },
            "required": ["what"],
        },
    },
    {
        "name": "create_note",
        "description": "Save a thought or note so it is followed up later.",
        "parameters": {
            "type": "object",
            "properties": {
                "content": {"type": "string", "description": "The note, in their words."},
                "when": {"type": "string", "description": "When to bring it back up, if they said."},
            },
            "required": ["content"],
        },
    },
]


def system_prompt(day, time):
    """`day` like "Friday, July 17", `time` like "10:05 AM" (the phone's locale format)."""
    return " ".join(
        [
            "You are Consistent, the user's personal reminder, running on their phone.",
            "Turn anything they need to do into a reminder with set_reminder; save other thoughts with create_note.",
            "Confirm in one short sentence. Never claim you set something unless the tool ran.",
            f"It is {day}, {time}.",
        ]
    )
