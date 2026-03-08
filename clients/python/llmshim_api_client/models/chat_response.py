from collections.abc import Mapping
from typing import Any, TypeVar, Optional, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..types import UNSET, Unset
from typing import cast
from typing import cast, Union
from typing import Union

if TYPE_CHECKING:
  from ..models.usage import Usage
  from ..models.response_message import ResponseMessage





T = TypeVar("T", bound="ChatResponse")



@_attrs_define
class ChatResponse:
    """ 
        Attributes:
            id (str): Response ID from the provider
            model (str):
            provider (str): Which provider handled the request
            message (ResponseMessage):
            usage (Usage):
            latency_ms (int): End-to-end latency in milliseconds
            reasoning (Union[None, Unset, str]): Reasoning/thinking content if the model produced it
     """

    id: str
    model: str
    provider: str
    message: 'ResponseMessage'
    usage: 'Usage'
    latency_ms: int
    reasoning: Union[None, Unset, str] = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)





    def to_dict(self) -> dict[str, Any]:
        from ..models.usage import Usage
        from ..models.response_message import ResponseMessage
        id = self.id

        model = self.model

        provider = self.provider

        message = self.message.to_dict()

        usage = self.usage.to_dict()

        latency_ms = self.latency_ms

        reasoning: Union[None, Unset, str]
        if isinstance(self.reasoning, Unset):
            reasoning = UNSET
        else:
            reasoning = self.reasoning


        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update({
            "id": id,
            "model": model,
            "provider": provider,
            "message": message,
            "usage": usage,
            "latency_ms": latency_ms,
        })
        if reasoning is not UNSET:
            field_dict["reasoning"] = reasoning

        return field_dict



    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.usage import Usage
        from ..models.response_message import ResponseMessage
        d = dict(src_dict)
        id = d.pop("id")

        model = d.pop("model")

        provider = d.pop("provider")

        message = ResponseMessage.from_dict(d.pop("message"))




        usage = Usage.from_dict(d.pop("usage"))




        latency_ms = d.pop("latency_ms")

        def _parse_reasoning(data: object) -> Union[None, Unset, str]:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(Union[None, Unset, str], data)

        reasoning = _parse_reasoning(d.pop("reasoning", UNSET))


        chat_response = cls(
            id=id,
            model=model,
            provider=provider,
            message=message,
            usage=usage,
            latency_ms=latency_ms,
            reasoning=reasoning,
        )


        chat_response.additional_properties = d
        return chat_response

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
