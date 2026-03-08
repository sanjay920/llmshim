from collections.abc import Mapping
from typing import Any, TypeVar, Optional, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..models.message_role import MessageRole
from ..types import UNSET, Unset
from typing import cast
from typing import cast, Union
from typing import Union

if TYPE_CHECKING:
  from ..models.message_content_type_1_item import MessageContentType1Item
  from ..models.tool_call import ToolCall





T = TypeVar("T", bound="Message")



@_attrs_define
class Message:
    """ 
        Attributes:
            role (MessageRole):
            content (Union[None, Unset, list['MessageContentType1Item'], str]): Text content or array of content blocks
            tool_call_id (Union[Unset, str]): For tool role messages, the ID of the tool call being responded to
            tool_calls (Union[Unset, list['ToolCall']]): Tool calls made by the assistant
     """

    role: MessageRole
    content: Union[None, Unset, list['MessageContentType1Item'], str] = UNSET
    tool_call_id: Union[Unset, str] = UNSET
    tool_calls: Union[Unset, list['ToolCall']] = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)





    def to_dict(self) -> dict[str, Any]:
        from ..models.message_content_type_1_item import MessageContentType1Item
        from ..models.tool_call import ToolCall
        role = self.role.value

        content: Union[None, Unset, list[dict[str, Any]], str]
        if isinstance(self.content, Unset):
            content = UNSET
        elif isinstance(self.content, list):
            content = []
            for content_type_1_item_data in self.content:
                content_type_1_item = content_type_1_item_data.to_dict()
                content.append(content_type_1_item)


        else:
            content = self.content

        tool_call_id = self.tool_call_id

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
        })
        if content is not UNSET:
            field_dict["content"] = content
        if tool_call_id is not UNSET:
            field_dict["tool_call_id"] = tool_call_id
        if tool_calls is not UNSET:
            field_dict["tool_calls"] = tool_calls

        return field_dict



    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.message_content_type_1_item import MessageContentType1Item
        from ..models.tool_call import ToolCall
        d = dict(src_dict)
        role = MessageRole(d.pop("role"))




        def _parse_content(data: object) -> Union[None, Unset, list['MessageContentType1Item'], str]:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            try:
                if not isinstance(data, list):
                    raise TypeError()
                content_type_1 = []
                _content_type_1 = data
                for content_type_1_item_data in (_content_type_1):
                    content_type_1_item = MessageContentType1Item.from_dict(content_type_1_item_data)



                    content_type_1.append(content_type_1_item)

                return content_type_1
            except: # noqa: E722
                pass
            return cast(Union[None, Unset, list['MessageContentType1Item'], str], data)

        content = _parse_content(d.pop("content", UNSET))


        tool_call_id = d.pop("tool_call_id", UNSET)

        tool_calls = []
        _tool_calls = d.pop("tool_calls", UNSET)
        for tool_calls_item_data in (_tool_calls or []):
            tool_calls_item = ToolCall.from_dict(tool_calls_item_data)



            tool_calls.append(tool_calls_item)


        message = cls(
            role=role,
            content=content,
            tool_call_id=tool_call_id,
            tool_calls=tool_calls,
        )


        message.additional_properties = d
        return message

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
