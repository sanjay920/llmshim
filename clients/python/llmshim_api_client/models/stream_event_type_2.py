from collections.abc import Mapping
from typing import Any, TypeVar, Optional, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..types import UNSET, Unset
from typing import Literal, Union, cast
from typing import Union






T = TypeVar("T", bound="StreamEventType2")



@_attrs_define
class StreamEventType2:
    """ 
        Attributes:
            type_ (Union[Literal['tool_call'], Unset]):
            id (Union[Unset, str]):
            name (Union[Unset, str]):
            arguments (Union[Unset, str]):
     """

    type_: Union[Literal['tool_call'], Unset] = UNSET
    id: Union[Unset, str] = UNSET
    name: Union[Unset, str] = UNSET
    arguments: Union[Unset, str] = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)





    def to_dict(self) -> dict[str, Any]:
        type_ = self.type_

        id = self.id

        name = self.name

        arguments = self.arguments


        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update({
        })
        if type_ is not UNSET:
            field_dict["type"] = type_
        if id is not UNSET:
            field_dict["id"] = id
        if name is not UNSET:
            field_dict["name"] = name
        if arguments is not UNSET:
            field_dict["arguments"] = arguments

        return field_dict



    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        type_ = cast(Union[Literal['tool_call'], Unset] , d.pop("type", UNSET))
        if type_ != 'tool_call'and not isinstance(type_, Unset):
            raise ValueError(f"type must match const 'tool_call', got '{type_}'")

        id = d.pop("id", UNSET)

        name = d.pop("name", UNSET)

        arguments = d.pop("arguments", UNSET)

        stream_event_type_2 = cls(
            type_=type_,
            id=id,
            name=name,
            arguments=arguments,
        )


        stream_event_type_2.additional_properties = d
        return stream_event_type_2

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
