from http import HTTPStatus
from typing import Any, Optional, Union, cast

import httpx

from ...client import AuthenticatedClient, Client
from ...types import Response, UNSET
from ... import errors

from ...models.chat_request import ChatRequest
from ...models.stream_event_type_0 import StreamEventType0
from ...models.stream_event_type_1 import StreamEventType1
from ...models.stream_event_type_2 import StreamEventType2
from ...models.stream_event_type_3 import StreamEventType3
from ...models.stream_event_type_4 import StreamEventType4
from ...models.stream_event_type_5 import StreamEventType5
from typing import cast
from typing import cast, Union



def _get_kwargs(
    *,
    body: ChatRequest,

) -> dict[str, Any]:
    headers: dict[str, Any] = {}


    

    

    _kwargs: dict[str, Any] = {
        "method": "post",
        "url": "/v1/chat/stream",
    }

    _kwargs["json"] = body.to_dict()


    headers["Content-Type"] = "application/json"

    _kwargs["headers"] = headers
    return _kwargs



def _parse_response(*, client: Union[AuthenticatedClient, Client], response: httpx.Response) -> Optional[Union['StreamEventType0', 'StreamEventType1', 'StreamEventType2', 'StreamEventType3', 'StreamEventType4', 'StreamEventType5']]:
    if response.status_code == 200:
        def _parse_response_200(data: object) -> Union['StreamEventType0', 'StreamEventType1', 'StreamEventType2', 'StreamEventType3', 'StreamEventType4', 'StreamEventType5']:
            try:
                if not isinstance(data, dict):
                    raise TypeError()
                componentsschemas_stream_event_type_0 = StreamEventType0.from_dict(data)



                return componentsschemas_stream_event_type_0
            except: # noqa: E722
                pass
            try:
                if not isinstance(data, dict):
                    raise TypeError()
                componentsschemas_stream_event_type_1 = StreamEventType1.from_dict(data)



                return componentsschemas_stream_event_type_1
            except: # noqa: E722
                pass
            try:
                if not isinstance(data, dict):
                    raise TypeError()
                componentsschemas_stream_event_type_2 = StreamEventType2.from_dict(data)



                return componentsschemas_stream_event_type_2
            except: # noqa: E722
                pass
            try:
                if not isinstance(data, dict):
                    raise TypeError()
                componentsschemas_stream_event_type_3 = StreamEventType3.from_dict(data)



                return componentsschemas_stream_event_type_3
            except: # noqa: E722
                pass
            try:
                if not isinstance(data, dict):
                    raise TypeError()
                componentsschemas_stream_event_type_4 = StreamEventType4.from_dict(data)



                return componentsschemas_stream_event_type_4
            except: # noqa: E722
                pass
            if not isinstance(data, dict):
                raise TypeError()
            componentsschemas_stream_event_type_5 = StreamEventType5.from_dict(data)



            return componentsschemas_stream_event_type_5

        response_200 = _parse_response_200(response.text)

        return response_200

    if client.raise_on_unexpected_status:
        raise errors.UnexpectedStatus(response.status_code, response.content)
    else:
        return None


def _build_response(*, client: Union[AuthenticatedClient, Client], response: httpx.Response) -> Response[Union['StreamEventType0', 'StreamEventType1', 'StreamEventType2', 'StreamEventType3', 'StreamEventType4', 'StreamEventType5']]:
    return Response(
        status_code=HTTPStatus(response.status_code),
        content=response.content,
        headers=response.headers,
        parsed=_parse_response(client=client, response=response),
    )


def sync_detailed(
    *,
    client: Union[AuthenticatedClient, Client],
    body: ChatRequest,

) -> Response[Union['StreamEventType0', 'StreamEventType1', 'StreamEventType2', 'StreamEventType3', 'StreamEventType4', 'StreamEventType5']]:
    """ Send a streaming chat completion request

     Always returns SSE events. Each event has a typed `event:` field
    (content, reasoning, tool_call, usage, done, error).

    Args:
        body (ChatRequest):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Union['StreamEventType0', 'StreamEventType1', 'StreamEventType2', 'StreamEventType3', 'StreamEventType4', 'StreamEventType5']]
     """


    kwargs = _get_kwargs(
        body=body,

    )

    response = client.get_httpx_client().request(
        **kwargs,
    )

    return _build_response(client=client, response=response)

def sync(
    *,
    client: Union[AuthenticatedClient, Client],
    body: ChatRequest,

) -> Optional[Union['StreamEventType0', 'StreamEventType1', 'StreamEventType2', 'StreamEventType3', 'StreamEventType4', 'StreamEventType5']]:
    """ Send a streaming chat completion request

     Always returns SSE events. Each event has a typed `event:` field
    (content, reasoning, tool_call, usage, done, error).

    Args:
        body (ChatRequest):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Union['StreamEventType0', 'StreamEventType1', 'StreamEventType2', 'StreamEventType3', 'StreamEventType4', 'StreamEventType5']
     """


    return sync_detailed(
        client=client,
body=body,

    ).parsed

async def asyncio_detailed(
    *,
    client: Union[AuthenticatedClient, Client],
    body: ChatRequest,

) -> Response[Union['StreamEventType0', 'StreamEventType1', 'StreamEventType2', 'StreamEventType3', 'StreamEventType4', 'StreamEventType5']]:
    """ Send a streaming chat completion request

     Always returns SSE events. Each event has a typed `event:` field
    (content, reasoning, tool_call, usage, done, error).

    Args:
        body (ChatRequest):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[Union['StreamEventType0', 'StreamEventType1', 'StreamEventType2', 'StreamEventType3', 'StreamEventType4', 'StreamEventType5']]
     """


    kwargs = _get_kwargs(
        body=body,

    )

    response = await client.get_async_httpx_client().request(
        **kwargs
    )

    return _build_response(client=client, response=response)

async def asyncio(
    *,
    client: Union[AuthenticatedClient, Client],
    body: ChatRequest,

) -> Optional[Union['StreamEventType0', 'StreamEventType1', 'StreamEventType2', 'StreamEventType3', 'StreamEventType4', 'StreamEventType5']]:
    """ Send a streaming chat completion request

     Always returns SSE events. Each event has a typed `event:` field
    (content, reasoning, tool_call, usage, done, error).

    Args:
        body (ChatRequest):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Union['StreamEventType0', 'StreamEventType1', 'StreamEventType2', 'StreamEventType3', 'StreamEventType4', 'StreamEventType5']
     """


    return (await asyncio_detailed(
        client=client,
body=body,

    )).parsed
