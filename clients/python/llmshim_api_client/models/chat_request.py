from collections.abc import Mapping
from typing import Any, TypeVar, Optional, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..types import UNSET, Unset
from typing import cast
from typing import Union

if TYPE_CHECKING:
  from ..models.config import Config
  from ..models.chat_request_provider_config import ChatRequestProviderConfig
  from ..models.message import Message





T = TypeVar("T", bound="ChatRequest")



@_attrs_define
class ChatRequest:
    """ 
        Attributes:
            model (str): Model identifier. Use "provider/model" format (e.g., "anthropic/claude-sonnet-4-6")
                or just the model name for auto-detection (e.g., "claude-sonnet-4-6").
            messages (list['Message']): Conversation messages
            stream (Union[Unset, bool]): If true on /v1/chat, returns SSE stream instead of JSON. Default: False.
            config (Union[Unset, Config]): Provider-agnostic configuration
            provider_config (Union[Unset, ChatRequestProviderConfig]): Raw provider-specific JSON merged into the underlying
                request.
                Use this for features like Anthropic thinking, Gemini safety settings, etc.
     """

    model: str
    messages: list['Message']
    stream: Union[Unset, bool] = False
    config: Union[Unset, 'Config'] = UNSET
    provider_config: Union[Unset, 'ChatRequestProviderConfig'] = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)





    def to_dict(self) -> dict[str, Any]:
        from ..models.config import Config
        from ..models.chat_request_provider_config import ChatRequestProviderConfig
        from ..models.message import Message
        model = self.model

        messages = []
        for messages_item_data in self.messages:
            messages_item = messages_item_data.to_dict()
            messages.append(messages_item)



        stream = self.stream

        config: Union[Unset, dict[str, Any]] = UNSET
        if not isinstance(self.config, Unset):
            config = self.config.to_dict()

        provider_config: Union[Unset, dict[str, Any]] = UNSET
        if not isinstance(self.provider_config, Unset):
            provider_config = self.provider_config.to_dict()


        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update({
            "model": model,
            "messages": messages,
        })
        if stream is not UNSET:
            field_dict["stream"] = stream
        if config is not UNSET:
            field_dict["config"] = config
        if provider_config is not UNSET:
            field_dict["provider_config"] = provider_config

        return field_dict



    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.config import Config
        from ..models.chat_request_provider_config import ChatRequestProviderConfig
        from ..models.message import Message
        d = dict(src_dict)
        model = d.pop("model")

        messages = []
        _messages = d.pop("messages")
        for messages_item_data in (_messages):
            messages_item = Message.from_dict(messages_item_data)



            messages.append(messages_item)


        stream = d.pop("stream", UNSET)

        _config = d.pop("config", UNSET)
        config: Union[Unset, Config]
        if isinstance(_config,  Unset):
            config = UNSET
        else:
            config = Config.from_dict(_config)




        _provider_config = d.pop("provider_config", UNSET)
        provider_config: Union[Unset, ChatRequestProviderConfig]
        if isinstance(_provider_config,  Unset):
            provider_config = UNSET
        else:
            provider_config = ChatRequestProviderConfig.from_dict(_provider_config)




        chat_request = cls(
            model=model,
            messages=messages,
            stream=stream,
            config=config,
            provider_config=provider_config,
        )


        chat_request.additional_properties = d
        return chat_request

    @property
    def additional_keys(self) -> list[str]:
        return list(self.additional_properties.keys())

    def __getitem__(self, key: str) -> Any:
        return self.additional_properties[key]

    def __setitem__(self, key: str, value: Any) -> None:
        self.additional_properties[key] = value

    def __delitem__(self, key: str) -> None:
        del self.additional_properties[key]

    def __contains__(self, key: str) -> bool:
        return key in self.additional_properties
