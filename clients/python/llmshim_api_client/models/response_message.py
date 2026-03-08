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
  from ..models.tool_call import ToolCall





T = TypeVar("T", bound="ResponseMessage")



@_attrs_define
class ResponseMessage:
    """ 
        Attributes:
            role (str):
            content (Union[None, str]): Text content or null
            tool_calls (Union[Unset, list['ToolCall']]):
     """

    role: str
    content: Union[None, str]
    tool_calls: Union[Unset, list['ToolCall']] = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)





    def to_dict(self) -> dict[str, Any]:
        from ..models.tool_call import ToolCall
        role = self.role

        content: Union[None, str]
        content = self.content

        tool_calls: Union[Unset, list[dict[str, Any]]] = UNSET
        if not isinstance(self.tool_calls, Unset):
            tool_calls = []
            for tool_calls_item_data in self.tool_calls:
                tool_calls_item = tool_calls_item_data.to_dict()
                tool_calls.append(tool_calls_item)




        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update({
            "role": role,
            "content": content,
        })
        if tool_calls is not UNSET:
            field_dict["tool_calls"] = tool_calls

        return field_dict



    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.tool_call import ToolCall
        d = dict(src_dict)
        role = d.pop("role")

        def _parse_content(data: object) -> Union[None, str]:
            if data is None:
                return data
            return cast(Union[None, str], data)

        content = _parse_content(d.pop("content"))


        tool_calls = []
        _tool_calls = d.pop("tool_calls", UNSET)
        for tool_calls_item_data in (_tool_calls or []):
            tool_calls_item = ToolCall.from_dict(tool_calls_item_data)



            tool_calls.append(tool_calls_item)


        response_message = cls(
            role=role,
            content=content,
            tool_calls=tool_calls,
        )


        response_message.additional_properties = d
        return response_message

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
