from collections.abc import Mapping
from typing import Any, TypeVar, Optional, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..types import UNSET, Unset
from typing import Literal, Union, cast
from typing import Union






T = TypeVar("T", bound="StreamEventType3")



@_attrs_define
class StreamEventType3:
    """ 
        Attributes:
            type_ (Union[Literal['usage'], Unset]):
            input_tokens (Union[Unset, int]):
            output_tokens (Union[Unset, int]):
            reasoning_tokens (Union[Unset, int]):
            total_tokens (Union[Unset, int]):
     """

    type_: Union[Literal['usage'], Unset] = UNSET
    input_tokens: Union[Unset, int] = UNSET
    output_tokens: Union[Unset, int] = UNSET
    reasoning_tokens: Union[Unset, int] = UNSET
    total_tokens: Union[Unset, int] = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)





    def to_dict(self) -> dict[str, Any]:
        type_ = self.type_

        input_tokens = self.input_tokens

        output_tokens = self.output_tokens

        reasoning_tokens = self.reasoning_tokens

        total_tokens = self.total_tokens


        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update({
        })
        if type_ is not UNSET:
            field_dict["type"] = type_
        if input_tokens is not UNSET:
            field_dict["input_tokens"] = input_tokens
        if output_tokens is not UNSET:
            field_dict["output_tokens"] = output_tokens
        if reasoning_tokens is not UNSET:
            field_dict["reasoning_tokens"] = reasoning_tokens
        if total_tokens is not UNSET:
            field_dict["total_tokens"] = total_tokens

        return field_dict



    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        type_ = cast(Union[Literal['usage'], Unset] , d.pop("type", UNSET))
        if type_ != 'usage'and not isinstance(type_, Unset):
            raise ValueError(f"type must match const 'usage', got '{type_}'")

        input_tokens = d.pop("input_tokens", UNSET)

        output_tokens = d.pop("output_tokens", UNSET)

        reasoning_tokens = d.pop("reasoning_tokens", UNSET)

        total_tokens = d.pop("total_tokens", UNSET)

        stream_event_type_3 = cls(
            type_=type_,
            input_tokens=input_tokens,
            output_tokens=output_tokens,
            reasoning_tokens=reasoning_tokens,
            total_tokens=total_tokens,
        )


        stream_event_type_3.additional_properties = d
        return stream_event_type_3

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
