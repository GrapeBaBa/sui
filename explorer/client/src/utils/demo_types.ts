import type { AnyVec, JsonBytes, SuiParentChildRef } from './rpc';

export type CosmeticOption = AnyVec | RawCosmetic;

export interface RawCosmetic {
    cosmetic_type: number;
    id: string;
}

export interface RawMonster {
    affection_level: number;
    applied_monster_cosmetic_0: CosmeticOption;
    applied_monster_cosmetic_1: CosmeticOption;
    breed: number;
    buddy_level: number;
    hunger_level: number;
    id: string;
    monster_affinity: number;
    monster_description: JsonBytes;
    monster_img_index: number;
    monster_level: number;
    monster_name: JsonBytes;
    monster_xp: number;
}

export type RawPetMonsters = SuiParentChildRef;

export interface RawFarm {
    applied_farm_cosmetic_0: CosmeticOption;
    applied_farm_cosmetic_1: CosmeticOption;
    current_xp: number;
    farm_img_index: number;
    farm_name: JsonBytes;
    id: string;
    level: number;
    occupied_monster_slots: number;
    pet_monsters: RawPetMonsters;
    total_monster_slots: number;
}
