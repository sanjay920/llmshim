from collections.abc import Mapping
from typing import Any, TypeVar, Optional, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..models.config_reasoning_effort import ConfigReasoningEffort
from ..types import UNSET, Unset
from typing import cast
from typing import Union






T = TypeVar("T", bound="Config")



@_attrs_define
class Config:
    """ Provider-agnostic configuration

        Attributes:
            max_tokens (Union[Unset, int]): Maximum output tokens
            temperature (Union[Unset, float]):
            top_p (Union[Unset, float]):
            top_k (Union[Unset, int]):
            stop (Union[Unset, list[str]]):
            reasoning_effort (Union[Unset, ConfigReasoningEffort]): Controls reasoning/thinking depth across all providers
     """

    max_tokens: Union[Unset, int] = UNSET
    temperature: Union[Unset, float] = UNSET
    top_p: Union[Unset, float] = UNSET
    top_k: Union[Unset, int] = UNSET
    stop: Union[Unset, list[str]] = UNSET
    reasoning_effort: Union[Unset, ConfigReasoningEffort] = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)





    def to_dict(self) -> dict[str, Any]:
        max_tokens = self.max_tokens

        temperature = self.temperature

        top_p = self.top_p

        top_k = self.top_k

        stop: Union[Unset, list[str]] = UNSET
        if not isinstance(self.stop, Unset):
            stop = self.stop



        reasoning_effort: Union[Unset, str] = UNSET
        if not isinstance(self.reasoning_effort, Unset):
            reasoning_effort = self.reasoning_effort.value



        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update({
        })
        if max_tokens is not UNSET:
            field_dict["max_tokens"] = max_tokens
        if temperature is not UNSET:
            field_dict["temperature"] = temperature
        if top_p is not UNSET:
            field_dict["top_p"] = top_p
        if top_k is not UNSET:
            field_dict["top_k"] = top_k
        if stop is not UNSET:
            field_dict["stop"] = stop
        if reasoning_effort is not UNSET:
            field_dict["reasoning_effort"] = reasoning_effort

        return field_dict



    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        max_tokens = d.pop("max_tokens", UNSET)

        temperature = d.pop("temperature", UNSET)

        top_p = d.pop("top_p", UNSET)

        top_k = d.pop("top_k", UNSET)

        stop = cast(list[str], d.pop("stop", UNSET))


        _reasoning_effort = d.pop("reasoning_effort", UNSET)
        reasoning_effort: Union[Unset, ConfigReasoningEffort]
        if isinstance(_reasoning_effort,  Unset):
            reasoning_effort = UNSET
        else:
            reasoning_effort = ConfigReasoningEffort(_reasoning_effort)




        config = cls(
            max_tokens=max_tokens,
            temperature=temperature,
            top_p=top_p,
            top_k=top_k,
            stop=stop,
            reasoning_effort=reasoning_effort,
        )


        config.additional_properties = d
        return config

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
