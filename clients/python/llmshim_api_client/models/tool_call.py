from collections.abc import Mapping
from typing import Any, TypeVar, Optional, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..models.tool_call_type import ToolCallType
from ..types import UNSET, Unset
from typing import cast
from typing import Union

if TYPE_CHECKING:
  from ..models.tool_call_function import ToolCallFunction





T = TypeVar("T", bound="ToolCall")



@_attrs_define
class ToolCall:
    """ 
        Attributes:
            id (Union[Unset, str]):
            type_ (Union[Unset, ToolCallType]):
            function (Union[Unset, ToolCallFunction]):
     """

    id: Union[Unset, str] = UNSET
    type_: Union[Unset, ToolCallType] = UNSET
    function: Union[Unset, 'ToolCallFunction'] = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)





    def to_dict(self) -> dict[str, Any]:
        from ..models.tool_call_function import ToolCallFunction
        id = self.id

        type_: Union[Unset, str] = UNSET
        if not isinstance(self.type_, Unset):
            type_ = self.type_.value


        function: Union[Unset, dict[str, Any]] = UNSET
        if not isinstance(self.function, Unset):
            function = self.function.to_dict()


        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update({
        })
        if id is not UNSET:
            field_dict["id"] = id
        if type_ is not UNSET:
            field_dict["type"] = type_
        if function is not UNSET:
            field_dict["function"] = function

        return field_dict



    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.tool_call_function import ToolCallFunction
        d = dict(src_dict)
        id = d.pop("id", UNSET)

        _type_ = d.pop("type", UNSET)
        type_: Union[Unset, ToolCallType]
        if isinstance(_type_,  Unset):
            type_ = UNSET
        else:
            type_ = ToolCallType(_type_)




        _function = d.pop("function", UNSET)
        function: Union[Unset, ToolCallFunction]
        if isinstance(_function,  Unset):
            function = UNSET
        else:
            function = ToolCallFunction.from_dict(_function)




        tool_call = cls(
            id=id,
            type_=type_,
            function=function,
        )


        tool_call.additional_properties = d
        return tool_call

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
