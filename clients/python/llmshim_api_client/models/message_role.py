from enum import Enum

class MessageRole(str, Enum):
    ASSISTANT = "assistant"
    DEVELOPER = "developer"
    SYSTEM = "system"
    TOOL = "tool"
    USER = "user"

    def __str__(self) -> str:
        return str(self.value)
