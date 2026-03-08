from collections.abc import Mapping
from typing import Any, TypeVar, Optional, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from typing import cast

if TYPE_CHECKING:
  from ..models.models_response_models_item import ModelsResponseModelsItem





T = TypeVar("T", bound="ModelsResponse")



@_attrs_define
class ModelsResponse:
    """ 
        Attributes:
            models (list['ModelsResponseModelsItem']):
     """

    models: list['ModelsResponseModelsItem']
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)





    def to_dict(self) -> dict[str, Any]:
        from ..models.models_response_models_item import ModelsResponseModelsItem
        models = []
        for models_item_data in self.models:
            models_item = models_item_data.to_dict()
            models.append(models_item)




        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update({
            "models": models,
        })

        return field_dict



    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.models_response_models_item import ModelsResponseModelsItem
        d = dict(src_dict)
        models = []
        _models = d.pop("models")
        for models_item_data in (_models):
            models_item = ModelsResponseModelsItem.from_dict(models_item_data)



            models.append(models_item)


        models_response = cls(
            models=models,
        )


        models_response.additional_properties = d
        return models_response

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
