<?xml version="1.0" encoding="UTF-8"?>
<Dataset xmlns:gml="http://www.opengis.net/gml/3.2"
    xmlns:s100_profile="http://www.iho.int/S-100/profile/s100_gmlProfile"
    xmlns:xlink="http://www.w3.org/1999/xlink"
    xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
    gml:id="S421.abc.abcde.00003"
    xmlns:S100="http://www.iho.int/s100gml/5.0"
    xmlns:S421="http://www.iec.ch/S421/2.0"
    xsi:schemaLocation="http://www.iec.ch/S421/2.0 ./S-421_schema_2.0.0.xsd"
    xmlns="http://www.iec.ch/S421/2.0"
>
    <gml:boundedBy>
        <gml:Envelope srsName="http://www.opengis.net/def/crs/EPSG/0/4326">
            <gml:lowerCorner>5.9225659 105.8900548</gml:lowerCorner>
            <gml:upperCorner>22.0758351 118.594834</gml:upperCorner>
        </gml:Envelope>
    </gml:boundedBy>
    <S100:DatasetIdentificationInformation>
        <S100:encodingSpecification>S-100 Part 10b</S100:encodingSpecification>
        <S100:encodingSpecificationEdition>1.0</S100:encodingSpecificationEdition>
        <S100:productIdentifier>S-421</S100:productIdentifier>
        <S100:productEdition>2.0.0</S100:productEdition>
        <S100:applicationProfile>S-421 GML</S100:applicationProfile>
        <S100:datasetFileIdentifier>S-421 Full generic test data</S100:datasetFileIdentifier>
        <S100:datasetTitle>S-421.Full.Test.Data</S100:datasetTitle>
        <S100:datasetReferenceDate>2026-06-17</S100:datasetReferenceDate>
        <S100:datasetLanguage>ENG</S100:datasetLanguage>
        <S100:datasetAbstract></S100:datasetAbstract>
        <S100:datasetTopicCategory>transportation</S100:datasetTopicCategory>
        <S100:datasetPurpose>base</S100:datasetPurpose>
        <S100:updateNumber>1</S100:updateNumber>
    </S100:DatasetIdentificationInformation>
    <members>
        <!-- IEC63173-1 12.3.4.1: valid baseline full generic route with extensions,
               schedules, waypoint legs and action points for positive import coverage.
               This baseline exercises representative optional content, but not every optional
               attribute in the schema is populated at the same time. -->
        <Route gml:id="RTE">
            <routeFormatVersion>2.0</routeFormatVersion>
            <routeID>9ce44ee8-5345-479c-b53d-43e34b97eba6</routeID>
            <routeEditionNo>1</routeEditionNo>
            <!-- IEC63173-1 12.3.4.1 a)-b): manufacturer extension baseline for full generic import tests. -->
            <routeExtensions>
                <routeExtensionsManufacturerId>11a</routeExtensionsManufacturerId>
                <routeExtensionsName>Test route extension</routeExtensionsName>
                <routeExtensionsVersion>1.0</routeExtensionsVersion>
                <routeExtensionsContent>Additional information of route</routeExtensionsContent>
            </routeExtensions>
            <routeInfo xlink:href="#RTE.INFO" xlink:title="routeInfoComposition" />
            <routeSchedules xlink:href="#RTE.SCHEDS" xlink:title="routeSchedulesComposition" />
            <routeWaypoints xlink:href="#RTE.WPTS" xlink:title="routeWaypointsComposition" />
            <routeActionPoints xlink:href="#RTE.APTS" xlink:title="routeActionPointsComposition" />
        </Route>
        <RouteInfo gml:id="RTE.INFO">
            <routeInfoName>S-421.Full.Test.Data</routeInfoName>
            <routeInfoAuthor>Andy Nibbs</routeInfoAuthor>
            <!-- IEC63173-1 12.3.4.1 b): valid UTC edition time for amended-route checks. -->
            <routeInfoEditionTime>2026-05-24T00:00:00Z</routeInfoEditionTime>
            <routeInfoDescription>Full data</routeInfoDescription>
            <routeInfoStatus code="2">Planned</routeInfoStatus>
            <routeInfoMasterApproval>false</routeInfoMasterApproval>
            <!-- IEC63173-1 12.3.4.2.1 b) Invalid primary departure port identifier format. -->
            <!-- <routeInfoDeparturePortID1>KR-PUS</routeInfoDeparturePortID1> -->
            <routeInfoDeparturePortID1>KRPUS</routeInfoDeparturePortID1>
            <!-- IEC63173-1 12.3.4.2.1 a) Invalid primary arrival port identifier format. -->
            <!-- <routeInfoArrivalPortID1>SG-CHG</routeInfoArrivalPortID1> -->
            <routeInfoArrivalPortID1>SGCHG</routeInfoArrivalPortID1>
            <!-- IEC63173-1 12.3.4.2.1 c) Invalid vessel type value. -->
            <!-- <routeInfoVesselType code="999">Invalid vessel type</routeInfoVesselType> -->
            <routeInfoVesselType code="76">Cargo ship, container ship</routeInfoVesselType>
            <routeInfoVesselName>HMS Liverpool</routeInfoVesselName>
            <!-- IEC63173-1 12.3.3.2.1 g) / 12.3.3.3.2 e): valid MMSI baseline and easy negative mutation. -->
            <!-- <routeInfoVesselMMSI>44012A456</routeInfoVesselMMSI> -->
            <routeInfoVesselMMSI>440123456</routeInfoVesselMMSI>
            <!-- IEC63173-1 12.3.4.2.1 d)-f):
                 this sample does not currently populate call sign, IMO number, vessel height, length or beam.
                 Add those optional fields here when preparing dedicated invalid-format full-route tests. -->
            <routeInfoVesselVoyage>From Busan to Changi June 2026</routeInfoVesselVoyage>
            <routeInfoCollection xlink:href="#RTE" xlink:title="routeInfoComposition" />
        </RouteInfo>
        <RouteSchedules gml:id="RTE.SCHEDS">
            <routeSchedule xlink:title="routeScheduleComposition" xlink:href="#RTE.SCHED.1" />
            <routeSchedulesCollection xlink:title="routeSchedulesComposition" xlink:href="#RTE" />
        </RouteSchedules>
        <RouteSchedule gml:id="RTE.SCHED.1">
            <!-- IEC63173-1 12.3.4.2.4 a)-b): duplicate this schedule block to create a reused
                 routeScheduleID, or retarget one schedule element below to a non-existent waypoint. -->
            <routeScheduleID>1</routeScheduleID>
            <routeScheduleName>Full TDS Schedule </routeScheduleName>
            <!-- IEC63173-1 12.3.4.2.4 a)-b): duplicate this schedule block to reuse routeScheduleID 1,
                 or retarget one schedule element below to a non-existent waypoint such as #RTE.WPT.999.
                 Calculated/recommended schedule variants can be derived from the same waypoint set when needed. -->
            <routeScheduleCollection xlink:href="#RTE.SCHEDS" xlink:title="routeScheduleComposition" />
            <routeScheduleManual xlink:title="routeScheduleManualComposition" xlink:href="#RTE.SCHED.1.MAN" />
        </RouteSchedule>
        <RouteScheduleManual gml:id="RTE.SCHED.1.MAN">
            <routeScheduleManualCollection xlink:href="#RTE.SCHED.1" xlink:title="routeScheduleManualComposition" />
            <routeScheduleElementManual xlink:title="routeScheduleElementManualComposition" xlink:href="#RTE.SCHED.1.MAN.ELEMENT.1" />
            <routeScheduleElementManual xlink:title="routeScheduleElementManualComposition" xlink:href="#RTE.SCHED.1.MAN.ELEMENT.2" />
            <routeScheduleElementManual xlink:title="routeScheduleElementManualComposition" xlink:href="#RTE.SCHED.1.MAN.ELEMENT.3" />
            <routeScheduleElementManual xlink:title="routeScheduleElementManualComposition" xlink:href="#RTE.SCHED.1.MAN.ELEMENT.4" />
            <routeScheduleElementManual xlink:title="routeScheduleElementManualComposition" xlink:href="#RTE.SCHED.1.MAN.ELEMENT.5" />
            <routeScheduleElementManual xlink:title="routeScheduleElementManualComposition" xlink:href="#RTE.SCHED.1.MAN.ELEMENT.6" />
            <routeScheduleElementManual xlink:title="routeScheduleElementManualComposition" xlink:href="#RTE.SCHED.1.MAN.ELEMENT.7" />
            <routeScheduleElementManual xlink:title="routeScheduleElementManualComposition" xlink:href="#RTE.SCHED.1.MAN.ELEMENT.8" />
            <routeScheduleElementManual xlink:title="routeScheduleElementManualComposition" xlink:href="#RTE.SCHED.1.MAN.ELEMENT.9" />
            <routeScheduleElementManual xlink:title="routeScheduleElementManualComposition" xlink:href="#RTE.SCHED.1.MAN.ELEMENT.10" />
        </RouteScheduleManual>
        <RouteScheduleElement gml:id="RTE.SCHED.1.MAN.ELEMENT.1">
            <!-- IEC63173-1 12.3.4.2.4 b)-f): change routeWaypointID or targetWaypoint to a non-existent
                 waypoint for the invalid-reference case, or use invalid ETD / ETD window values below. -->
            <routeWaypointID>1</routeWaypointID>
            <routeScheduleElementETD>2026-06-17T15:00:00Z</routeScheduleElementETD>
            <!-- <routeScheduleElementETD>2026-06-17T25:00:00Z</routeScheduleElementETD> -->
            <routeScheduleElementETDWindowBefore>60</routeScheduleElementETDWindowBefore>
            <routeScheduleElementETDWindowAfter>60</routeScheduleElementETDWindowAfter>
            <!-- <routeScheduleElementETDWindowBefore>-1</routeScheduleElementETDWindowBefore> -->
            <!-- <routeScheduleElementETDWindowAfter>-1</routeScheduleElementETDWindowAfter> -->
            <routeScheduleElementManualCollection xlink:href="#RTE.SCHED.1.MAN" xlink:title="routeScheduleElementManualComposition" />
            <targetWaypoint xlink:href="#RTE.WPT.1" xlink:title="routeWaypointSchedule" />
        </RouteScheduleElement>
        <RouteScheduleElement gml:id="RTE.SCHED.1.MAN.ELEMENT.2">
            <routeWaypointID>2</routeWaypointID>
            <routeScheduleElementPlanSOG>15.0</routeScheduleElementPlanSOG>
            <routeScheduleElementETD>2026-06-18T00:00:00Z</routeScheduleElementETD>
            <routeScheduleElementETA>2026-06-18T00:00:00Z</routeScheduleElementETA>
            <routeScheduleElementManualCollection xlink:href="#RTE.SCHED.1.MAN" xlink:title="routeScheduleElementManualComposition" />
            <targetWaypoint xlink:href="#RTE.WPT.2" xlink:title="routeWaypointSchedule" />
        </RouteScheduleElement>
        <RouteScheduleElement gml:id="RTE.SCHED.1.MAN.ELEMENT.3">
            <routeWaypointID>3</routeWaypointID>
            <routeScheduleElementPlanSOG>15.0</routeScheduleElementPlanSOG>
            <routeScheduleElementETD>2026-06-18T09:00:00Z</routeScheduleElementETD>
            <routeScheduleElementETA>2026-06-18T09:00:00Z</routeScheduleElementETA>
            <routeScheduleElementManualCollection xlink:href="#RTE.SCHED.1.MAN" xlink:title="routeScheduleElementManualComposition" />
            <targetWaypoint xlink:href="#RTE.WPT.3" xlink:title="routeWaypointSchedule" />
        </RouteScheduleElement>
        <RouteScheduleElement gml:id="RTE.SCHED.1.MAN.ELEMENT.4">
            <routeWaypointID>4</routeWaypointID>
            <routeScheduleElementPlanSOG>15.0</routeScheduleElementPlanSOG>
            <routeScheduleElementETD>2026-06-18T18:00:00Z</routeScheduleElementETD>
            <routeScheduleElementETA>2026-06-18T18:00:00Z</routeScheduleElementETA>
            <routeScheduleElementManualCollection xlink:href="#RTE.SCHED.1.MAN" xlink:title="routeScheduleElementManualComposition" />
            <targetWaypoint xlink:href="#RTE.WPT.4" xlink:title="routeWaypointSchedule" />
        </RouteScheduleElement>
        <RouteScheduleElement gml:id="RTE.SCHED.1.MAN.ELEMENT.5">
            <routeWaypointID>5</routeWaypointID>
            <routeScheduleElementPlanSOG>15.0</routeScheduleElementPlanSOG>
            <routeScheduleElementETD>2026-06-19T02:00:00Z</routeScheduleElementETD>
            <routeScheduleElementETA>2026-06-19T02:00:00Z</routeScheduleElementETA>
            <routeScheduleElementManualCollection xlink:href="#RTE.SCHED.1.MAN" xlink:title="routeScheduleElementManualComposition" />
            <targetWaypoint xlink:href="#RTE.WPT.5" xlink:title="routeWaypointSchedule" />
        </RouteScheduleElement>
        <RouteScheduleElement gml:id="RTE.SCHED.1.MAN.ELEMENT.6">
            <routeWaypointID>6</routeWaypointID>
            <routeScheduleElementPlanSOG>15.0</routeScheduleElementPlanSOG>
            <routeScheduleElementETD>2026-06-19T11:00:00Z</routeScheduleElementETD>
            <routeScheduleElementETA>2026-06-19T11:00:00Z</routeScheduleElementETA>
            <routeScheduleElementManualCollection xlink:href="#RTE.SCHED.1.MAN" xlink:title="routeScheduleElementManualComposition" />
            <targetWaypoint xlink:href="#RTE.WPT.6" xlink:title="routeWaypointSchedule" />
        </RouteScheduleElement>
        <RouteScheduleElement gml:id="RTE.SCHED.1.MAN.ELEMENT.7">
            <routeWaypointID>7</routeWaypointID>
            <routeScheduleElementPlanSOG>15.0</routeScheduleElementPlanSOG>
            <routeScheduleElementETD>2026-06-19T20:00:00Z</routeScheduleElementETD>
            <routeScheduleElementETA>2026-06-19T20:00:00Z</routeScheduleElementETA>
            <routeScheduleElementManualCollection xlink:href="#RTE.SCHED.1.MAN" xlink:title="routeScheduleElementManualComposition" />
            <targetWaypoint xlink:href="#RTE.WPT.7" xlink:title="routeWaypointSchedule" />
        </RouteScheduleElement>
        <RouteScheduleElement gml:id="RTE.SCHED.1.MAN.ELEMENT.8">
            <routeWaypointID>8</routeWaypointID>
            <routeScheduleElementPlanSOG>15.0</routeScheduleElementPlanSOG>
            <routeScheduleElementETD>2026-06-20T05:00:00Z</routeScheduleElementETD>
            <routeScheduleElementETA>2026-06-20T05:00:00Z</routeScheduleElementETA>
            <routeScheduleElementManualCollection xlink:href="#RTE.SCHED.1.MAN" xlink:title="routeScheduleElementManualComposition" />
            <targetWaypoint xlink:href="#RTE.WPT.8" xlink:title="routeWaypointSchedule" />
        </RouteScheduleElement>
        <RouteScheduleElement gml:id="RTE.SCHED.1.MAN.ELEMENT.9">
            <routeWaypointID>9</routeWaypointID>
            <routeScheduleElementPlanSOG>15.0</routeScheduleElementPlanSOG>
            <routeScheduleElementETD>2026-06-20T14:00:00Z</routeScheduleElementETD>
            <routeScheduleElementETA>2026-06-20T14:00:00Z</routeScheduleElementETA>
            <routeScheduleElementManualCollection xlink:href="#RTE.SCHED.1.MAN" xlink:title="routeScheduleElementManualComposition" />
            <targetWaypoint xlink:href="#RTE.WPT.9" xlink:title="routeWaypointSchedule" />
        </RouteScheduleElement>
        <RouteScheduleElement gml:id="RTE.SCHED.1.MAN.ELEMENT.10">
            <routeWaypointID>10</routeWaypointID>
            <routeScheduleElementPlanSOG>15.0</routeScheduleElementPlanSOG>
            <routeScheduleElementETA>2026-06-20T23:00:00Z</routeScheduleElementETA>
            <!-- <routeScheduleElementETA>2026-06-20T99:00:00Z</routeScheduleElementETA> -->
            <routeScheduleElementETAWindowBefore>60</routeScheduleElementETAWindowBefore>
            <routeScheduleElementETAWindowAfter>60</routeScheduleElementETAWindowAfter>
            <!-- <routeScheduleElementETAWindowBefore>-1</routeScheduleElementETAWindowBefore> -->
            <!-- <routeScheduleElementETAWindowAfter>-1</routeScheduleElementETAWindowAfter> -->
            <routeScheduleElementManualCollection xlink:href="#RTE.SCHED.1.MAN" xlink:title="routeScheduleElementManualComposition" />
            <targetWaypoint xlink:href="#RTE.WPT.10" xlink:title="routeWaypointSchedule" />
        </RouteScheduleElement>
        <RouteWaypoints gml:id="RTE.WPTS">
            <!-- IEC63173-1 12.2.4 / 12.3.4: keep all 10 waypoint references aligned with the
                 waypoint objects and schedule targets below when preparing positive full-route tests. -->
            <routeWaypointsCollection xlink:title="routeWaypointsComposition" xlink:href="#RTE" />
            <routeWaypoint xlink:title="routeWaypointComposition" xlink:href="#RTE.WPT.1" />
            <routeWaypoint xlink:title="routeWaypointComposition" xlink:href="#RTE.WPT.2" />
            <routeWaypoint xlink:title="routeWaypointComposition" xlink:href="#RTE.WPT.3" />
            <routeWaypoint xlink:title="routeWaypointComposition" xlink:href="#RTE.WPT.4" />
            <routeWaypoint xlink:title="routeWaypointComposition" xlink:href="#RTE.WPT.5" />
            <routeWaypoint xlink:title="routeWaypointComposition" xlink:href="#RTE.WPT.6" />
            <routeWaypoint xlink:title="routeWaypointComposition" xlink:href="#RTE.WPT.7" />
            <routeWaypoint xlink:title="routeWaypointComposition" xlink:href="#RTE.WPT.8" />
            <routeWaypoint xlink:title="routeWaypointComposition" xlink:href="#RTE.WPT.9" />
            <routeWaypoint xlink:title="routeWaypointComposition" xlink:href="#RTE.WPT.10" />
        </RouteWaypoints>
        <RouteWaypoint gml:id="RTE.WPT.1">
            <routeWaypointID>1</routeWaypointID>
            <routeWaypointName>JPNGO NE</routeWaypointName>
            <routeWaypointFixed>false</routeWaypointFixed>
            <routeWaypointTurnRadius>2.0</routeWaypointTurnRadius>
            <!-- IEC63173-1 12.3.4.1 a): waypoint extension baseline for full generic import. -->
            <routeWaypointExtensions>
                <routeExtensionsManufacturerId>11a</routeExtensionsManufacturerId>
                <routeExtensionsName>Test route waypoint-1</routeExtensionsName>
                <routeExtensionsVersion>1.0</routeExtensionsVersion>
                <routeExtensionsContent>Additional information of route waypoint-1</routeExtensionsContent>
            </routeWaypointExtensions>
            <scheduleOf xlink:href="#RTE.SCHED.1.MAN.ELEMENT.1" xlink:title="routeWaypointSchedule" />
            <routeWaypointCollection xlink:href="#RTE.WPTS" xlink:title="routeWaypointComposition" />
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.WPT.1.PT">
                        <gml:pos srsName="http://www.opengis.net/def/crs/EPSG/0/4326">22.0758351 118.594834</gml:pos>
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteWaypoint>
        <RouteWaypoint gml:id="RTE.WPT.2">
            <routeWaypointID>2</routeWaypointID>
            <routeWaypointName>SE1</routeWaypointName>
            <routeWaypointFixed>false</routeWaypointFixed>
            <routeWaypointTurnRadius>2.0</routeWaypointTurnRadius>
            <routeWaypointExtensions>
                <routeExtensionsManufacturerId>11a</routeExtensionsManufacturerId>
                <routeExtensionsName>Test route waypoint-2</routeExtensionsName>
                <routeExtensionsVersion>1.0</routeExtensionsVersion>
                <routeExtensionsContent>Additional information of route waypoint-2</routeExtensionsContent>
            </routeWaypointExtensions>
            <scheduleOf xlink:href="#RTE.SCHED.1.MAN.ELEMENT.2" xlink:title="routeWaypointSchedule" />
            <routeWaypointCollection xlink:href="#RTE.WPTS" xlink:title="routeWaypointComposition" />
            <routeWaypointLeg xlink:title="routeWaypointLegComposition" xlink:href="#RTE.WPT.LEG.1" />
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.WPT.2.PT">
                        <gml:pos srsName="http://www.opengis.net/def/crs/EPSG/0/4326">19.2995235 116.176209</gml:pos>
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteWaypoint>
        <RouteWaypoint gml:id="RTE.WPT.3">
            <routeWaypointID>3</routeWaypointID>
            <routeWaypointName>SE2</routeWaypointName>
            <routeWaypointFixed>false</routeWaypointFixed>
            <routeWaypointTurnRadius>2.0</routeWaypointTurnRadius>
            <routeWaypointExtensions>
                <routeExtensionsManufacturerId>11a</routeExtensionsManufacturerId>
                <routeExtensionsName>Test route waypoint-3</routeExtensionsName>
                <routeExtensionsVersion>1.0</routeExtensionsVersion>
                <routeExtensionsContent>Additional information of route waypoint-3</routeExtensionsContent>
            </routeWaypointExtensions>
            <scheduleOf xlink:href="#RTE.SCHED.1.MAN.ELEMENT.3" xlink:title="routeWaypointSchedule" />
            <routeWaypointCollection xlink:href="#RTE.WPTS" xlink:title="routeWaypointComposition" />
            <routeWaypointLeg xlink:title="routeWaypointLegComposition" xlink:href="#RTE.WPT.LEG.2" />
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.WPT.3.PT">
                        <gml:pos srsName="http://www.opengis.net/def/crs/EPSG/0/4326">16.5628522 114.3051923</gml:pos>
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteWaypoint>
        <RouteWaypoint gml:id="RTE.WPT.4">
            <routeWaypointID>4</routeWaypointID>
            <routeWaypointName>SE3</routeWaypointName>
            <routeWaypointFixed>false</routeWaypointFixed>
            <routeWaypointTurnRadius>2.0</routeWaypointTurnRadius>
            <routeWaypointExtensions>
                <routeExtensionsManufacturerId>11a</routeExtensionsManufacturerId>
                <routeExtensionsName>Test route waypoint-4</routeExtensionsName>
                <routeExtensionsVersion>1.0</routeExtensionsVersion>
                <routeExtensionsContent>Additional information of route waypoint-4</routeExtensionsContent>
            </routeWaypointExtensions>
            <scheduleOf xlink:href="#RTE.SCHED.1.MAN.ELEMENT.4" xlink:title="routeWaypointSchedule" />
            <routeWaypointCollection xlink:href="#RTE.WPTS" xlink:title="routeWaypointComposition" />
            <routeWaypointLeg xlink:title="routeWaypointLegComposition" xlink:href="#RTE.WPT.LEG.3" />
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.WPT.4.PT">
                        <gml:pos srsName="http://www.opengis.net/def/crs/EPSG/0/4326">13.3872954 112.7079853</gml:pos>
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteWaypoint>
        <RouteWaypoint gml:id="RTE.WPT.5">
            <routeWaypointID>5</routeWaypointID>
            <routeWaypointName>SE4</routeWaypointName>
            <routeWaypointFixed>false</routeWaypointFixed>
            <routeWaypointTurnRadius>2.0</routeWaypointTurnRadius>
            <routeWaypointExtensions>
                <routeExtensionsManufacturerId>11a</routeExtensionsManufacturerId>
                <routeExtensionsName>Test route waypoint-5</routeExtensionsName>
                <routeExtensionsVersion>1.0</routeExtensionsVersion>
                <routeExtensionsContent>Additional information of route waypoint-5</routeExtensionsContent>
            </routeWaypointExtensions>
            <scheduleOf xlink:href="#RTE.SCHED.1.MAN.ELEMENT.5" xlink:title="routeWaypointSchedule" />
            <routeWaypointCollection xlink:href="#RTE.WPTS" xlink:title="routeWaypointComposition" />
            <routeWaypointLeg xlink:title="routeWaypointLegComposition" xlink:href="#RTE.WPT.LEG.4" />
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.WPT.5.PT">
                        <gml:pos srsName="http://www.opengis.net/def/crs/EPSG/0/4326">8.9535741 109.6961093</gml:pos>
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteWaypoint>
        <RouteWaypoint gml:id="RTE.WPT.6">
            <routeWaypointID>6</routeWaypointID>
            <routeWaypointName>SE5</routeWaypointName>
            <routeWaypointFixed>false</routeWaypointFixed>
            <routeWaypointTurnRadius>2.0</routeWaypointTurnRadius>
            <routeWaypointExtensions>
                <routeExtensionsManufacturerId>11a</routeExtensionsManufacturerId>
                <routeExtensionsName>Test route waypoint-6</routeExtensionsName>
                <routeExtensionsVersion>1.0</routeExtensionsVersion>
                <routeExtensionsContent>Additional information of route waypoint-6</routeExtensionsContent>
            </routeWaypointExtensions>
            <scheduleOf xlink:href="#RTE.SCHED.1.MAN.ELEMENT.6" xlink:title="routeWaypointSchedule" />
            <routeWaypointCollection xlink:href="#RTE.WPTS" xlink:title="routeWaypointComposition" />
            <routeWaypointLeg xlink:title="routeWaypointLegComposition" xlink:href="#RTE.WPT.LEG.5" />
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.WPT.6.PT">
                        <gml:pos srsName="http://www.opengis.net/def/crs/EPSG/0/4326">5.9225659 105.8900548</gml:pos>
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteWaypoint>
        <RouteWaypoint gml:id="RTE.WPT.7">
            <routeWaypointID>7</routeWaypointID>
            <routeWaypointName>SE6</routeWaypointName>
            <routeWaypointFixed>false</routeWaypointFixed>
            <routeWaypointTurnRadius>2.0</routeWaypointTurnRadius>
            <routeWaypointExtensions>
                <routeExtensionsManufacturerId>11a</routeExtensionsManufacturerId>
                <routeExtensionsName>Test route waypoint-7</routeExtensionsName>
                <routeExtensionsVersion>1.0</routeExtensionsVersion>
                <routeExtensionsContent>Additional information of route waypoint-7</routeExtensionsContent>
            </routeWaypointExtensions>
            <scheduleOf xlink:href="#RTE.SCHED.1.MAN.ELEMENT.7" xlink:title="routeWaypointSchedule" />
            <routeWaypointCollection xlink:href="#RTE.WPTS" xlink:title="routeWaypointComposition" />
            <routeWaypointLeg xlink:href="#RTE.WPT.LEG.6" xlink:title="routeWaypointLegComposition" />
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.WPT.7.PT" srsName="http://www.opengis.net/def/crs/EPSG/0/4326" srsDimension="2">
                        <gml:pos>9.6963651 112.6541345</gml:pos>
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteWaypoint>
        <RouteWaypoint gml:id="RTE.WPT.8">
            <routeWaypointID>8</routeWaypointID>
            <routeWaypointName>SE7</routeWaypointName>
            <routeWaypointFixed>false</routeWaypointFixed>
            <routeWaypointTurnRadius>2.0</routeWaypointTurnRadius>
            <routeWaypointExtensions>
                <routeExtensionsManufacturerId>11a</routeExtensionsManufacturerId>
                <routeExtensionsName>Test route waypoint-8</routeExtensionsName>
                <routeExtensionsVersion>1.0</routeExtensionsVersion>
                <routeExtensionsContent>Additional information of route waypoint-8</routeExtensionsContent>
            </routeWaypointExtensions>
            <scheduleOf xlink:href="#RTE.SCHED.1.MAN.ELEMENT.8" xlink:title="routeWaypointSchedule" />
            <routeWaypointCollection xlink:href="#RTE.WPTS" xlink:title="routeWaypointComposition" />
            <routeWaypointLeg xlink:href="#RTE.WPT.LEG.7" xlink:title="routeWaypointLegComposition" />
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.WPT.8.PT" srsName="http://www.opengis.net/def/crs/EPSG/0/4326" srsDimension="2">
                        <gml:pos>7.6109595 110.8081315</gml:pos>
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteWaypoint>
        <RouteWaypoint gml:id="RTE.WPT.9">
            <routeWaypointID>9</routeWaypointID>
            <routeWaypointName>SE8</routeWaypointName>
            <routeWaypointFixed>false</routeWaypointFixed>
            <routeWaypointTurnRadius>2.0</routeWaypointTurnRadius>
            <routeWaypointExtensions>
                <routeExtensionsManufacturerId>11a</routeExtensionsManufacturerId>
                <routeExtensionsName>Test route waypoint-9</routeExtensionsName>
                <routeExtensionsVersion>1.0</routeExtensionsVersion>
                <routeExtensionsContent>Additional information of route waypoint-9</routeExtensionsContent>
            </routeWaypointExtensions>
            <scheduleOf xlink:href="#RTE.SCHED.1.MAN.ELEMENT.9" xlink:title="routeWaypointSchedule" />
            <routeWaypointCollection xlink:href="#RTE.WPTS" xlink:title="routeWaypointComposition" />
            <routeWaypointLeg xlink:href="#RTE.WPT.LEG.8" xlink:title="routeWaypointLegComposition" />
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.WPT.9.PT" srsName="http://www.opengis.net/def/crs/EPSG/0/4326" srsDimension="2">
                        <gml:pos>6.069203 108.6691139</gml:pos>
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteWaypoint>
        <RouteWaypoint gml:id="RTE.WPT.10">
            <routeWaypointID>10</routeWaypointID>
            <routeWaypointName>SE9</routeWaypointName>
            <routeWaypointFixed>false</routeWaypointFixed>
            <routeWaypointTurnRadius>2.0</routeWaypointTurnRadius>
            <routeWaypointExtensions>
                <routeExtensionsManufacturerId>11a</routeExtensionsManufacturerId>
                <routeExtensionsName>Test route waypoint-10</routeExtensionsName>
                <routeExtensionsVersion>1.0</routeExtensionsVersion>
                <routeExtensionsContent>Additional information of route waypoint-10</routeExtensionsContent>
            </routeWaypointExtensions>
            <scheduleOf xlink:href="#RTE.SCHED.1.MAN.ELEMENT.10" xlink:title="routeWaypointSchedule" />
            <routeWaypointCollection xlink:href="#RTE.WPTS" xlink:title="routeWaypointComposition" />
            <routeWaypointLeg xlink:href="#RTE.WPT.LEG.9" xlink:title="routeWaypointLegComposition" />
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.WPT.10.PT" srsName="http://www.opengis.net/def/crs/EPSG/0/4326" srsDimension="2">
                        <gml:pos>4.4938137 105.9733644</gml:pos>
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteWaypoint>
           <!-- IEC63173-1 12.3.4.2.3 d)-j): use the first leg below for invalid geometry-type,
               XTDL/check-limit, or SOG min/max negative cases before copying the same mutation pattern
               to additional legs if a broader scenario is needed. -->
           <RouteWaypointLeg gml:id="RTE.WPT.LEG.1">
            <routeWaypointLegStarboardXTDL>1</routeWaypointLegStarboardXTDL>
            <routeWaypointLegPortXTDL>1</routeWaypointLegPortXTDL>
            <routeWaypointLegGeometryType>loxodrome</routeWaypointLegGeometryType>
            <routeWaypointLegSOGMin>10</routeWaypointLegSOGMin>
            <routeWaypointLegCollection xlink:href="#RTE.WPT.2" xlink:title="routeWaypointLegComposition" />
        </RouteWaypointLeg>
        <RouteWaypointLeg gml:id="RTE.WPT.LEG.2">
            <routeWaypointLegStarboardXTDL>1</routeWaypointLegStarboardXTDL>
            <routeWaypointLegPortXTDL>1</routeWaypointLegPortXTDL>
            <routeWaypointLegGeometryType>loxodrome</routeWaypointLegGeometryType>
            <routeWaypointLegSOGMin>10</routeWaypointLegSOGMin>
            <routeWaypointLegCollection xlink:href="#RTE.WPT.3" xlink:title="routeWaypointLegComposition" />
        </RouteWaypointLeg>
        <RouteWaypointLeg gml:id="RTE.WPT.LEG.3">
            <routeWaypointLegStarboardXTDL>1</routeWaypointLegStarboardXTDL>
            <routeWaypointLegPortXTDL>1</routeWaypointLegPortXTDL>
            <routeWaypointLegGeometryType>loxodrome</routeWaypointLegGeometryType>
            <routeWaypointLegSOGMin>10</routeWaypointLegSOGMin>
            <routeWaypointLegCollection xlink:href="#RTE.WPT.4" xlink:title="routeWaypointLegComposition" />
        </RouteWaypointLeg>
        <RouteWaypointLeg gml:id="RTE.WPT.LEG.4">
            <routeWaypointLegStarboardXTDL>1</routeWaypointLegStarboardXTDL>
            <routeWaypointLegPortXTDL>1</routeWaypointLegPortXTDL>
            <routeWaypointLegGeometryType>loxodrome</routeWaypointLegGeometryType>
            <routeWaypointLegSOGMin>10</routeWaypointLegSOGMin>
            <routeWaypointLegCollection xlink:href="#RTE.WPT.5" xlink:title="routeWaypointLegComposition" />
        </RouteWaypointLeg>
        <RouteWaypointLeg gml:id="RTE.WPT.LEG.5">
            <routeWaypointLegStarboardXTDL>1</routeWaypointLegStarboardXTDL>
            <routeWaypointLegPortXTDL>1</routeWaypointLegPortXTDL>
            <routeWaypointLegGeometryType>loxodrome</routeWaypointLegGeometryType>
            <routeWaypointLegSOGMin>10</routeWaypointLegSOGMin>
            <routeWaypointLegCollection xlink:href="#RTE.WPT.6" xlink:title="routeWaypointLegComposition" />
        </RouteWaypointLeg>
        <RouteWaypointLeg gml:id="RTE.WPT.LEG.6">
            <routeWaypointLegStarboardXTDL>1</routeWaypointLegStarboardXTDL>
            <routeWaypointLegPortXTDL>1</routeWaypointLegPortXTDL>
            <routeWaypointLegGeometryType>loxodrome</routeWaypointLegGeometryType>
            <routeWaypointLegSOGMin>10</routeWaypointLegSOGMin>
            <routeWaypointLegCollection xlink:href="#RTE.WPT.7" xlink:title="routeWaypointLegComposition" />
        </RouteWaypointLeg>
        <RouteWaypointLeg gml:id="RTE.WPT.LEG.7">
            <routeWaypointLegStarboardXTDL>1</routeWaypointLegStarboardXTDL>
            <routeWaypointLegPortXTDL>1</routeWaypointLegPortXTDL>
            <routeWaypointLegGeometryType>loxodrome</routeWaypointLegGeometryType>
            <routeWaypointLegSOGMin>10</routeWaypointLegSOGMin>
            <routeWaypointLegCollection xlink:href="#RTE.WPT.8" xlink:title="routeWaypointLegComposition" />
        </RouteWaypointLeg>
        <RouteWaypointLeg gml:id="RTE.WPT.LEG.8">
            <routeWaypointLegStarboardXTDL>1</routeWaypointLegStarboardXTDL>
            <routeWaypointLegPortXTDL>1</routeWaypointLegPortXTDL>
            <routeWaypointLegGeometryType>loxodrome</routeWaypointLegGeometryType>
            <routeWaypointLegSOGMin>10</routeWaypointLegSOGMin>
            <routeWaypointLegCollection xlink:href="#RTE.WPT.9" xlink:title="routeWaypointLegComposition" />
        </RouteWaypointLeg>
        <RouteWaypointLeg gml:id="RTE.WPT.LEG.9">
            <routeWaypointLegStarboardXTDL>1</routeWaypointLegStarboardXTDL>
            <routeWaypointLegPortXTDL>1</routeWaypointLegPortXTDL>
            <routeWaypointLegGeometryType>loxodrome</routeWaypointLegGeometryType>
            <routeWaypointLegSOGMin>10</routeWaypointLegSOGMin>
            <routeWaypointLegCollection xlink:href="#RTE.WPT.10" xlink:title="routeWaypointLegComposition" />
        </RouteWaypointLeg>
        <RouteActionPoints gml:id="RTE.APTS">
            <routeActionPointsCollection xlink:title="routeActionPointsComposition" xlink:href="#RTE" />
            <routeActionPoint xlink:title="routeActionPointComposition" xlink:href="#RTE.APTS.1" />
        </RouteActionPoints>
        <RouteActionPoint gml:id="RTE.APTS.1">
            <!-- IEC63173-1 12.3.4.2.2 a): duplicate this block and keep routeActionPointID 1
                 to create the duplicated-action-point-id negative case. -->
            <routeActionPointID>1</routeActionPointID>
            <routeActionPointName>VTS Report</routeActionPointName>
            <routeActionPointTimeToAct>10</routeActionPointTimeToAct>
            <!-- IEC63173-1 12.3.4.1 a): valid action point action-code baseline for full import tests. -->
            <routeActionPointRequiredAction code="2">Report</routeActionPointRequiredAction>
            <routeActionPointRequiredActionDescription>VTS Report</routeActionPointRequiredActionDescription>
            <routeActionPointCollection xlink:href="#RTE.APTS" xlink:title="routeActionPointComposition" />
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.APT.1.PT" srsName="http://www.opengis.net/def/crs/EPSG/0/4326" srsDimension="2">
                        <gml:pos>10.9409139 112.8873593</gml:pos>
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteActionPoint>
    </members>
</Dataset>