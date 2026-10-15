import pandas as pd
from typing import (
    Dict,
    Sequence,
    Optional,
    List,
    Tuple,
    final,
    Union,
    Protocol,
    type_check_only,
    TypedDict
)

@type_check_only
class VoiceData(TypedDict):
    tick: int
    steamid: int
    bytes: bytes

@type_check_only
class WantedPropStateProtocol(Protocol):
    prop: str
    state: Union[bool, str, int, float]

@final
class WantedPropState:
    def __new__(cls, prop: str, state: Union[bool, str, int, float]) -> WantedPropState: ...

@final
class DemoParser:
    def __new__(cls, demo_path: str) -> DemoParser: ...
    def parse_header(self) -> Dict[str, str]: ...
    def list_updated_fields(self) -> List[str]: ...
    def list_game_events(self) -> List[str]: ...
    def parse_grenades(
        self,
        *,
        extra: Optional[Sequence[str]] = None,
        grenades: bool = True,
        grenade_classes: Optional[Sequence[str]] = None,
        skip_nones: bool = False,
    ) -> pd.DataFrame: ...
    def parse_player_info(self) -> pd.DataFrame: ...
    def parse_item_drops(self) -> pd.DataFrame: ...
    def parse_skins(self) -> pd.DataFrame: ...
    def parse_event(
        self,
        event_name: str,
        *,
        player: Optional[Sequence[str]] = None,
        other: Optional[Sequence[str]] = None,
    ) -> pd.DataFrame: ...
    def parse_events(
        self,
        event_name: Sequence[str],
        *,
        player: Optional[Sequence[str]] = None,
        other: Optional[Sequence[str]] = None,
    ) -> List[Tuple[str, pd.DataFrame]]: ...
    def parse_voice(self) -> List[VoiceData]: ...
    def parse_ticks(
        self,
        wanted_props: Sequence[str],
        *,
        players: Optional[Sequence[int]] = None,
        ticks: Optional[Sequence[int]] = None,
        prop_states: Optional[
            Sequence[WantedPropStateProtocol | WantedPropState]
        ] = None,
    ) -> pd.DataFrame:
        """Parse the specified props.

        Args:
            wanted_props (Sequence[str]): The props to parse for each player at each tick.
            player (Optional[Sequence[int]]): Sequence of Steam IDs of the players to parse.
                `None` or an empty Sequence means all players. Defaults to `None`.
            ticks (Optional[Sequence[int]]): Sequence of ticks to parse.
                `None` or an empty Sequence means all ticks. Defaults to `None`.

        Returns:
            pd.DataFrame: Dataframe of all the parsed props for each player at each tick.
        """
    def parse_tick_range(
        self,
        wanted_props: Sequence[str],
        tick_lo: int,
        tick_hi: int,
        *,
        players: Optional[Sequence[int]] = None,
    ) -> pd.DataFrame:
        """Parse the specified props for ticks in ``[tick_lo, tick_hi]``.

        Only the fullpacket segments overlapping the range are decoded, so
        peak memory stays proportional to the slice — never the whole demo.
        Same DataFrame shape as :meth:`parse_ticks`.

        Args:
            wanted_props (Sequence[str]): The props to parse for each player at each tick.
            tick_lo (int): First tick (inclusive).
            tick_hi (int): Last tick (inclusive).
            players (Optional[Sequence[int]]): Sequence of Steam IDs of the players to parse.
                `None` or an empty Sequence means all players. Defaults to `None`.

        Returns:
            pd.DataFrame: Dataframe of the parsed props for each player at each tick in range.
        """

__all__ = ["DemoParser", "WantedPropState"]
